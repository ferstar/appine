//! overlayfs (Linux 6.1's `fs/overlayfs`, the kernel/vfs map): a filesystem that shows several
//! store directories (layers) merged into one tree. The lower layers are read-only; a name in a
//! higher layer hides the same name below it, a whiteout (a character device 0:0) hides it
//! without showing anything, and a directory marked opaque (`trusted.overlay.opaque` = `y`, or
//! `user.overlay.opaque` with `userxattr`) is not merged with the directories below it.
//!
//! Every layer is a directory in the inode store, so an overlay node refers to the store inodes
//! behind it ([`OvlNode`]) and reads go to the topmost of them. Nodes are created as names are
//! looked up and kept per directory and name (Linux's overlay dentries), so the same path always
//! gives the same node; they are dropped with their instance ([`Overlays::remove_instance`]).

use std::collections::HashMap;

use super::store::{DirEntry, Ino, Stat, Store, mode};
use crate::errno::Errno;

/// An overlay node's number (in [`super::Node::Overlay`]), unique across all instances.
pub(crate) type OvlId = u64;

/// An instance's layers: the upper layer's root directory (None for a read-only overlay) and the
/// lower layers' root directories, topmost first.
pub(crate) type Layers = (Option<Ino>, Vec<Ino>);

/// Most layers one overlay may stack (Linux's `OVL_MAX_STACK`).
const MAX_STACK: usize = 500;

/// Every overlay instance and the nodes looked up in them.
#[derive(Default)]
pub(crate) struct Overlays {
    /// Instances by device number (bind mounts of an overlay share their source's).
    instances: HashMap<u64, Instance>,
    nodes: HashMap<OvlId, OvlNode>,
    next: OvlId,
}

/// One overlay instance (Linux's overlay superblock).
pub(crate) struct Instance {
    /// The upper layer's root directory, where changes go; None for a read-only overlay.
    pub upper: Option<Ino>,
    /// Options as given.
    pub options: Options,
}

/// An overlay node: the store inodes behind one name (Linux's `ovl_entry`).
pub(crate) struct OvlNode {
    /// Device number of its instance.
    pub dev: u64,
    /// The directory it was found in; the root's is itself.
    pub parent: OvlId,
    /// The name it was found under; empty for the root.
    pub name: Vec<u8>,
    /// The inode in the upper layer.
    pub upper: Option<Ino>,
    /// Inodes in the lower layers, topmost first: for a directory every lower directory merged
    /// into it, for anything else only the topmost one, and none when the upper inode hides them.
    pub lower: Vec<Ino>,
    /// Nodes already looked up in it, by name.
    children: HashMap<Vec<u8>, OvlId>,
}

impl OvlNode {
    /// The store inode reads go to: the upper one, or the topmost lower one.
    pub(crate) fn real(&self) -> Ino {
        self.upper.or(self.lower.first().copied()).unwrap_or(0)
    }

    /// Whether it is a merged directory: more than one layer shows a directory under its name
    /// (Linux's `OVL_TYPE_MERGE`).
    fn merged(&self) -> bool {
        self.lower.len() + usize::from(self.upper.is_some()) > 1
    }
}

/// mount(2) options of an overlay (Linux 6.1's `ovl_config`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Options {
    /// `lowerdir=` as given (escaped `:` included).
    pub lowerdir: Vec<u8>,
    /// `upperdir=` as given.
    pub upperdir: Option<Vec<u8>>,
    /// `workdir=` as given.
    pub workdir: Option<Vec<u8>>,
    /// `redirect_dir=` when given.
    pub redirect_dir: Option<Vec<u8>>,
    pub default_permissions: bool,
    /// `index=`, `nfs_export=`, `xino=`, `metacopy=`, `uuid=` when given.
    pub index: Option<bool>,
    pub nfs_export: Option<bool>,
    pub xino: Option<Vec<u8>>,
    pub metacopy: Option<bool>,
    pub uuid: Option<bool>,
    pub volatile: bool,
    /// The private attributes are `user.overlay.*` instead of `trusted.overlay.*`.
    pub userxattr: bool,
}

impl Options {
    /// Parses mount(2)'s data as 6.1's `ovl_parse_opt` does: options are separated by commas
    /// that no backslash escapes, empty ones are skipped, anything unknown or without its value
    /// is EINVAL, and a later option replaces an earlier one.
    pub(crate) fn parse(data: &[u8]) -> Result<Options, Errno> {
        let mut o = Options::default();
        let mut redirect_given = false;
        for opt in split_escaped(data, b',') {
            if opt.is_empty() {
                continue;
            }
            let (key, value) = match opt.iter().position(|&b| b == b'=') {
                Some(i) => (&opt[..i], Some(&opt[i + 1..])),
                None => (&opt[..], None),
            };
            let on_off = |v: Option<&[u8]>| match v {
                Some(b"on") => Ok(true),
                Some(b"off") => Ok(false),
                _ => Err(Errno::EINVAL),
            };
            match (key, value) {
                (b"lowerdir", Some(v)) => o.lowerdir = v.to_vec(),
                (b"upperdir", Some(v)) => o.upperdir = Some(v.to_vec()),
                (b"workdir", Some(v)) => o.workdir = Some(v.to_vec()),
                (b"default_permissions", None) => o.default_permissions = true,
                (b"redirect_dir", Some(v)) => {
                    if !matches!(v, b"on" | b"off" | b"follow" | b"nofollow") {
                        return Err(Errno::EINVAL);
                    }
                    o.redirect_dir = Some(v.to_vec());
                    redirect_given = true;
                }
                (b"index", v) => o.index = Some(on_off(v)?),
                (b"uuid", v) => o.uuid = Some(on_off(v)?),
                (b"nfs_export", v) => o.nfs_export = Some(on_off(v)?),
                (b"userxattr", None) => o.userxattr = true,
                (b"xino", Some(v @ (b"on" | b"off" | b"auto"))) => o.xino = Some(v.to_vec()),
                (b"metacopy", v) => o.metacopy = Some(on_off(v)?),
                (b"volatile", None) => o.volatile = true,
                _ => return Err(Errno::EINVAL),
            }
        }
        // Workdir, index and volatile mean nothing without an upper layer (ignored, as in 6.1)
        if o.upperdir.is_none() {
            o.workdir = None;
            o.index = None;
            o.volatile = false;
        }
        // metacopy=on needs redirect_dir, nfs_export=on needs index and excludes metacopy, and
        // userxattr excludes all three
        let redirect_off = matches!(o.redirect_dir.as_deref(), Some(b"off" | b"nofollow"));
        if o.metacopy == Some(true) && redirect_given && redirect_off {
            return Err(Errno::EINVAL);
        }
        if o.nfs_export == Some(true) && (o.index == Some(false) || o.metacopy == Some(true)) {
            return Err(Errno::EINVAL);
        }
        if o.userxattr
            && (o.index == Some(true)
                || o.metacopy == Some(true)
                || o.redirect_dir.as_deref() == Some(b"on"))
        {
            return Err(Errno::EINVAL);
        }
        Ok(o)
    }

    /// The lower layers' paths: `lowerdir=` split at colons no backslash escapes, unescaped
    /// (6.1's `ovl_split_lowerdirs` and `ovl_unescape`). EINVAL for an empty one or more than
    /// [`MAX_STACK`].
    pub(crate) fn lower_paths(&self) -> Result<Vec<Vec<u8>>, Errno> {
        if self.lowerdir.is_empty() {
            return Err(Errno::EINVAL);
        }
        let paths: Vec<Vec<u8>> = split_escaped(&self.lowerdir, b':')
            .map(|p| unescape(&p))
            .collect();
        if paths.len() > MAX_STACK || paths.iter().any(Vec::is_empty) {
            return Err(Errno::EINVAL);
        }
        Ok(paths)
    }

    /// The superblock options mountinfo shows after `ro`/`rw` (6.1's `ovl_show_options`):
    /// `lowerdir`, `upperdir` and `workdir` as given, then what differs from the defaults
    /// (redirect_dir follow, index, nfs_export, metacopy and xino off).
    pub(crate) fn show(&self) -> String {
        let mut s = String::new();
        let mut add = |key: &str, value: &[u8]| {
            s.push(',');
            s.push_str(key);
            if !value.is_empty() {
                s.push('=');
                s.push_str(&escape_option(value));
            }
        };
        add("lowerdir", &self.lowerdir);
        if let (Some(upper), Some(work)) = (&self.upperdir, &self.workdir) {
            add("upperdir", upper);
            add("workdir", work);
        }
        if self.default_permissions {
            add("default_permissions", b"");
        }
        if let Some(r) = self.redirect_dir.as_deref().filter(|r| *r != b"follow") {
            add("redirect_dir", r);
        }
        if self.index == Some(true) {
            add("index", b"on");
        }
        if self.uuid == Some(false) {
            add("uuid", b"off");
        }
        if self.nfs_export == Some(true) {
            add("nfs_export", b"on");
        }
        if self.metacopy == Some(true) {
            add("metacopy", b"on");
        }
        if self.volatile {
            add("volatile", b"");
        }
        if self.userxattr {
            add("userxattr", b"");
        }
        s
    }

    /// The prefix of the attributes overlayfs keeps for itself.
    pub(crate) fn private_prefix(&self) -> &'static [u8] {
        if self.userxattr {
            b"user.overlay."
        } else {
            b"trusted.overlay."
        }
    }
}

/// An option's value as mountinfo shows it (Linux's `seq_show_option`): commas, equals signs,
/// spaces, tabs, newlines and backslashes as octal escapes.
fn escape_option(value: &[u8]) -> String {
    let mut out = Vec::with_capacity(value.len());
    for &b in value {
        match b {
            b',' | b'=' | b' ' | b'\t' | b'\n' | b'\\' => {
                out.extend_from_slice(format!("\\{b:03o}").as_bytes())
            }
            _ => out.push(b),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits at `sep` where no backslash escapes it (the backslashes stay).
fn split_escaped(s: &[u8], sep: u8) -> impl Iterator<Item = Vec<u8>> + '_ {
    let mut rest = Some(s);
    std::iter::from_fn(move || {
        let s = rest?;
        let mut i = 0;
        while i < s.len() {
            match s[i] {
                b'\\' => i += 2,
                b if b == sep => {
                    rest = Some(&s[i + 1..]);
                    return Some(s[..i].to_vec());
                }
                _ => i += 1,
            }
        }
        rest = None;
        Some(s.to_vec())
    })
}

/// Removes the backslashes escaping the next byte.
fn unescape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.iter();
    while let Some(&b) = it.next() {
        match b {
            b'\\' => out.extend(it.next()),
            _ => out.push(b),
        }
    }
    out
}

/// A whiteout: a character device with device number 0:0.
fn is_whiteout(st: &Stat) -> bool {
    st.kind() == mode::S_IFCHR && st.rdev == 0
}

/// A directory entry that may be a whiteout (only character devices can be).
fn maybe_whiteout(store: &Store, e: &DirEntry) -> Result<bool, Errno> {
    Ok(e.mode & mode::S_IFMT == mode::S_IFCHR && is_whiteout(&store.attrs(e.ino)?))
}

impl Overlays {
    /// Records a new instance with device number `dev` and returns its root node. `upper` is the
    /// upper layer's root, `lower` the lower layers' roots, topmost first.
    pub(crate) fn add_instance(
        &mut self,
        dev: u64,
        upper: Option<Ino>,
        lower: Vec<Ino>,
        options: Options,
    ) -> OvlId {
        let root = self.add_node(OvlNode {
            dev,
            parent: 0,
            name: Vec::new(),
            upper,
            lower,
            children: HashMap::new(),
        });
        self.node_mut(root).parent = root;
        self.instances.insert(dev, Instance { upper, options });
        root
    }

    /// Forgets instance `dev` and all its nodes (its last mount is gone).
    pub(crate) fn remove_instance(&mut self, dev: u64) {
        if self.instances.remove(&dev).is_some() {
            self.nodes.retain(|_, n| n.dev != dev);
        }
    }

    /// Device numbers of the instances.
    pub(crate) fn devs(&self) -> Vec<u64> {
        self.instances.keys().copied().collect()
    }

    /// Instance `dev`.
    pub(crate) fn instance(&self, dev: u64) -> Option<&Instance> {
        self.instances.get(&dev)
    }

    /// Node `id`.
    pub(crate) fn node(&self, id: OvlId) -> &OvlNode {
        &self.nodes[&id]
    }

    fn node_mut(&mut self, id: OvlId) -> &mut OvlNode {
        self.nodes.get_mut(&id).expect("overlay node")
    }

    fn add_node(&mut self, node: OvlNode) -> OvlId {
        self.next += 1;
        self.nodes.insert(self.next, node);
        self.next
    }

    /// The options of the instance node `id` is in.
    pub(crate) fn options_of(&self, id: OvlId) -> &Options {
        &self.instances[&self.node(id).dev].options
    }

    /// Looks up `name` (not `.` or `..`) in directory `dir` (6.1's `ovl_lookup`): the upper layer
    /// first, then each lower directory merged into `dir`, stopping at a whiteout, at anything
    /// that is not a directory, and after an opaque directory; something that is not a directory
    /// below a directory is not seen. ENOENT when no layer has the name.
    pub(crate) fn lookup(
        &mut self,
        store: &Store,
        dir: OvlId,
        name: &[u8],
    ) -> Result<OvlId, Errno> {
        let d = self.node(dir);
        let prefix = self.options_of(dir).private_prefix();
        let opaque = |ino: Ino| -> Result<bool, Errno> {
            match store.get_xattr(ino, &[prefix, b"opaque"].concat()) {
                Ok(v) => Ok(v == b"y"),
                Err(Errno::ENODATA) => Ok(false),
                Err(e) => Err(e),
            }
        };
        let mut upper = None;
        let mut stop = false;
        if let Some(u) = d.upper
            && let Some(c) = store.lookup(u, name)?
        {
            let st = store.attrs(c)?;
            if is_whiteout(&st) {
                return Err(Errno::ENOENT);
            }
            upper = Some(c);
            stop = !st.is_dir() || opaque(c)?;
        }
        let mut lower = Vec::new();
        if !stop {
            for &l in &d.lower {
                let Some(c) = store.lookup(l, name)? else {
                    continue;
                };
                let st = store.attrs(c)?;
                if is_whiteout(&st) {
                    break;
                }
                if !st.is_dir() {
                    if upper.is_none() && lower.is_empty() {
                        lower.push(c);
                    }
                    break;
                }
                lower.push(c);
                if opaque(c)? {
                    break;
                }
            }
        }
        if upper.is_none() && lower.is_empty() {
            return Err(Errno::ENOENT);
        }
        let dev = d.dev;
        // The same name gives the same node again, brought up to date with the layers
        if let Some(&id) = d.children.get(name) {
            let n = self.node_mut(id);
            n.upper = upper;
            n.lower = lower;
            return Ok(id);
        }
        let id = self.add_node(OvlNode {
            dev,
            parent: dir,
            name: name.to_vec(),
            upper,
            lower,
            children: HashMap::new(),
        });
        self.node_mut(dir).children.insert(name.to_vec(), id);
        Ok(id)
    }

    /// The entries of directory `dir`, without `.` and `..` and sorted by name (6.1's
    /// `ovl_dir_read_merged`): each name from the highest layer that has it, whiteouts hiding the
    /// names below them, each entry's inode number the one stat shows.
    pub(crate) fn readdir(&self, store: &Store, dir: OvlId) -> Result<Vec<DirEntry>, Errno> {
        let d = self.node(dir);
        let layers = d.upper.iter().chain(&d.lower);
        let mut seen: std::collections::BTreeMap<Vec<u8>, Option<DirEntry>> = Default::default();
        for &layer in layers {
            for e in store.readdir(layer)? {
                if seen.contains_key(&e.name) {
                    continue;
                }
                let shown = (!maybe_whiteout(store, &e)?).then(|| e.clone());
                seen.insert(e.name, shown);
            }
        }
        Ok(seen.into_values().flatten().collect())
    }

    /// Metadata of node `id` (6.1's `ovl_getattr`): the topmost inode's, with the inode number
    /// of the upper inode or else the topmost lower one, and one link for a merged directory.
    pub(crate) fn stat(&self, store: &Store, id: OvlId) -> Result<Stat, Errno> {
        let n = self.node(id);
        let mut st = store.stat(n.real())?;
        if st.is_dir() && n.merged() {
            st.nlink = 1;
        }
        Ok(st)
    }

    /// The node's parent directory (the root's is itself).
    pub(crate) fn parent(&self, id: OvlId) -> OvlId {
        self.node(id).parent
    }

    /// Whether `name` is one of the attributes the instance of node `id` keeps for itself
    /// (`trusted.overlay.*`, or `user.overlay.*` with `userxattr`), which reads through the
    /// overlay never see.
    pub(crate) fn is_private_xattr(&self, id: OvlId, name: &[u8]) -> bool {
        name.starts_with(self.options_of(id).private_prefix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_options_as_linux() {
        let o = Options::parse(b"lowerdir=a\\:b:c,,index=off,xino=auto").unwrap();
        assert_eq!(
            o.lower_paths().unwrap(),
            vec![b"a:b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(o.index, None, "index means nothing without an upper layer");
        assert_eq!(Options::parse(b"lowerdir=a,bogus"), Err(Errno::EINVAL));
        assert_eq!(Options::parse(b"lowerdir"), Err(Errno::EINVAL));
        assert_eq!(Options::parse(b"redirect_dir=maybe"), Err(Errno::EINVAL));
        let o = Options::parse(b"lowerdir=a:,upperdir=u").unwrap();
        assert_eq!(o.lower_paths(), Err(Errno::EINVAL));
        assert_eq!(o.workdir, None);
        let o = Options::parse(b"upperdir=u,workdir=w,lowerdir=l,userxattr,volatile").unwrap();
        assert_eq!(
            o.show(),
            ",lowerdir=l,upperdir=u,workdir=w,volatile,userxattr"
        );
        assert_eq!(o.private_prefix(), b"user.overlay.");
        assert_eq!(
            Options::parse(b"lowerdir=l,upperdir=u,userxattr,index=on"),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn merges_layers() {
        let dir = std::env::temp_dir().join(format!("appine-ovl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::create(&dir).unwrap();
        let root = super::super::store::ROOT;
        let a = s.mkdir(root, b"a", 0o755, 0, 0).unwrap();
        let b = s.mkdir(root, b"b", 0o755, 0, 0).unwrap();
        let ad = s.mkdir(a, b"d", 0o755, 0, 0).unwrap();
        let bd = s.mkdir(b, b"d", 0o755, 0, 0).unwrap();
        let ao = s.mkdir(a, b"o", 0o755, 0, 0).unwrap();
        let bo = s.mkdir(b, b"o", 0o755, 0, 0).unwrap();
        s.set_xattr(ao, b"trusted.overlay.opaque", b"y", false, false)
            .unwrap();
        let (af, _) = s.create_file(a, b"f", 0o644, 0, 0).unwrap();
        s.create_file(b, b"f", 0o644, 0, 0).unwrap();
        s.create_file(ad, b"x", 0o644, 0, 0).unwrap();
        s.create_file(bd, b"y", 0o644, 0, 0).unwrap();
        s.create_file(bo, b"z", 0o644, 0, 0).unwrap();
        s.mknod(a, b"gone", mode::S_IFCHR, 0, 0, 0).unwrap();
        s.create_file(b, b"gone", 0o644, 0, 0).unwrap();
        // A directory in a lower layer hidden by a file above it
        s.create_file(a, b"g", 0o644, 0, 0).unwrap();
        s.mkdir(b, b"g", 0o755, 0, 0).unwrap();
        let mut o = Overlays::default();
        let top = o.add_instance(0x30, None, vec![a, b], Options::default());
        let names = |o: &Overlays, d| -> Vec<Vec<u8>> {
            o.readdir(&s, d)
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        assert_eq!(names(&o, top), [&b"d"[..], b"f", b"g", b"o"]);
        let d = o.lookup(&s, top, b"d").unwrap();
        assert_eq!(o.node(d).lower, [ad, bd]);
        assert_eq!(names(&o, d), [b"x", b"y"]);
        assert_eq!(o.stat(&s, d).unwrap().nlink, 1);
        assert_eq!(o.stat(&s, d).unwrap().ino, ad);
        let od = o.lookup(&s, top, b"o").unwrap();
        assert_eq!(names(&o, od), [b"z"; 0]);
        assert_eq!(o.lookup(&s, od, b"z"), Err(Errno::ENOENT));
        assert_eq!(o.lookup(&s, top, b"gone"), Err(Errno::ENOENT));
        let f = o.lookup(&s, top, b"f").unwrap();
        assert_eq!(o.node(f).real(), af);
        let g = o.lookup(&s, top, b"g").unwrap();
        assert_eq!(o.node(g).lower.len(), 1);
        // Looked up again: the same node
        assert_eq!(o.lookup(&s, top, b"d").unwrap(), d);
        o.remove_instance(0x30);
        assert!(o.nodes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
