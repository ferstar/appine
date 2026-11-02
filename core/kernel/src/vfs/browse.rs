//! 宿主浏览仓库（App 的 DocumentsProvider，设计文档 §11.3）：不经过内核，直接在仓库里按
//! guest 路径查找、列目录、打开文件，在 App 的家目录里新建、写、删除、改名、移动。
//!
//! 看到的是仓库里的样子，也就是内核没在运行时的样子：挂载（procfs、`/dev`、`/run` 的
//! tmpfs、bind 挂载）都不管，`/proc` 这些是仓库里的空目录。符号链接跟随，绝对路径从 guest
//! 的根算起，与 guest 里看到的一样。只给出目录和普通文件：设备节点、FIFO、socket 和断了的
//! 符号链接不列出来，也打不开（宿主读 FIFO 会一直等）。
//!
//! 不检查 guest 的权限：仓库里的一切本来就属于 App 的用户，在终端里 `doas` 也都看得到。

use std::fs::File;

use super::store::{Ino, ROOT, Stat, Store, mode};
use crate::errno::Errno;

/// 跟随符号链接的次数上限，与 Linux 的 `MAXSYMLINKS` 相同。
const MAX_LINKS: usize = 40;

/// 目录里的一项：名字和它（符号链接跟随之后）的元数据。
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: Vec<u8>,
    pub stat: Stat,
}

/// 按 guest 路径找到 inode，符号链接都跟随（包括最后一个组件）。`..` 在根目录上不动。
pub fn resolve(store: &Store, path: &[u8]) -> Result<Ino, Errno> {
    walk(store, path).map(|(ino, _)| ino)
}

/// [`resolve`]，同时给出真实路径的组件（符号链接都换成了它指向的地方，像 realpath）。
fn walk(store: &Store, path: &[u8]) -> Result<(Ino, Vec<Vec<u8>>), Errno> {
    // 还没走的组件，倒着放（栈顶是下一个）；跟随符号链接时把目标的组件压进去
    let mut pending: Vec<Vec<u8>> = components(path).rev().collect();
    // 走到的目录：真实路径的组件和各自的 inode（`..` 退一层就是真正的上级）
    let mut real: Vec<Vec<u8>> = Vec::new();
    let mut inos: Vec<Ino> = Vec::new();
    let mut links = 0;
    while let Some(name) = pending.pop() {
        let dir = inos.last().copied().unwrap_or(ROOT);
        if store.kind(dir)? != mode::S_IFDIR {
            return Err(Errno::ENOTDIR);
        }
        if name == b".." {
            real.pop();
            inos.pop();
            continue;
        }
        let ino = store.lookup(dir, &name)?.ok_or(Errno::ENOENT)?;
        if store.kind(ino)? == mode::S_IFLNK {
            links += 1;
            if links > MAX_LINKS {
                return Err(Errno::ELOOP);
            }
            let target = store.readlink(ino)?;
            if target.starts_with(b"/") {
                real.clear();
                inos.clear();
            }
            pending.extend(components(&target).rev());
            continue;
        }
        real.push(name);
        inos.push(ino);
    }
    Ok((inos.last().copied().unwrap_or(ROOT), real))
}

/// 路径的元数据（符号链接跟随）。不是目录、普通文件的是 ENOENT。
pub fn stat(store: &Store, path: &[u8]) -> Result<Stat, Errno> {
    let st = store.stat(resolve(store, path)?)?;
    if !shown(&st) {
        return Err(Errno::ENOENT);
    }
    Ok(st)
}

/// 列出目录 `dir`，按名字排序。符号链接给出目标的元数据，断了的、指向特殊文件的不列。
pub fn list(store: &Store, dir: &[u8]) -> Result<Vec<Entry>, Errno> {
    let ino = resolve(store, dir)?;
    if store.kind(ino)? != mode::S_IFDIR {
        return Err(Errno::ENOTDIR);
    }
    let mut out = Vec::new();
    for e in store.readdir(ino)? {
        let st = match e.mode & mode::S_IFMT {
            mode::S_IFLNK => {
                let mut path = dir.to_vec();
                path.push(b'/');
                path.extend_from_slice(&e.name);
                match resolve(store, &path).and_then(|i| store.stat(i)) {
                    Ok(st) => st,
                    Err(_) => continue,
                }
            }
            _ => match store.stat(e.ino) {
                Ok(st) => st,
                Err(_) => continue,
            },
        };
        if shown(&st) {
            out.push(Entry {
                name: e.name,
                stat: st,
            });
        }
    }
    Ok(out)
}

/// 打开普通文件读：返回宿主上的 object 文件。目录是 EISDIR。
pub fn open(store: &Store, path: &[u8]) -> Result<File, Errno> {
    let ino = resolve(store, path)?;
    match store.kind(ino)? {
        mode::S_IFREG => store.open_object(ino, false),
        mode::S_IFDIR => Err(Errno::EISDIR),
        _ => Err(Errno::ENOENT),
    }
}

/// 在目录 `dir` 下面（不含它自己）找名字里有 `query` 的（不分大小写），最多 `limit` 个，
/// 按广度优先：浅的在前。返回路径（从 `dir` 开始拼）和元数据。不进符号链接指向的目录，
/// 免得绕圈；最多看 `budget` 项，大目录树里不会一直找下去。
pub fn search(
    store: &Store,
    dir: &[u8],
    query: &str,
    limit: usize,
    budget: usize,
) -> Result<Vec<(Vec<u8>, Stat)>, Errno> {
    let query = query.to_lowercase();
    let root = resolve(store, dir)?;
    let mut queue = std::collections::VecDeque::from([(root, dir.to_vec())]);
    let mut out = Vec::new();
    let mut seen = 0;
    while let Some((ino, path)) = queue.pop_front() {
        for e in store.readdir(ino)? {
            seen += 1;
            if seen > budget {
                return Ok(out);
            }
            let mut child = path.clone();
            if !child.ends_with(b"/") {
                child.push(b'/');
            }
            child.extend_from_slice(&e.name);
            if e.mode & mode::S_IFMT == mode::S_IFDIR {
                queue.push_back((e.ino, child.clone()));
            }
            if !String::from_utf8_lossy(&e.name)
                .to_lowercase()
                .contains(&query)
            {
                continue;
            }
            if let Ok(st) = stat(store, &child) {
                out.push((child, st));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

// ---- 修改（只在 App 的家目录里面）----
//
// 写的都限制在 `top`（App 的登录账号的家目录）里面：按真实路径算，经由符号链接走到外面去的
// 不行（EACCES）。要删除、改名的目录项本身不跟随（删掉的是链接，不是它指向的东西），它所在
// 的目录跟随。

/// 一次新建时换名字最多试这么多次（`a (1).txt`、`a (2).txt`……）。
const MAX_TRIES: u32 = 100;

/// `top` 的真实路径的组件，它要是目录。
fn top_of(store: &Store, top: &[u8]) -> Result<Vec<Vec<u8>>, Errno> {
    let (ino, real) = walk(store, top)?;
    if store.kind(ino)? != mode::S_IFDIR {
        return Err(Errno::ENOTDIR);
    }
    Ok(real)
}

/// `top` 里面的目录 `dir`（符号链接跟随）。
fn dir_in(store: &Store, top: &[u8], dir: &[u8]) -> Result<Ino, Errno> {
    let top = top_of(store, top)?;
    let (ino, real) = walk(store, dir)?;
    if store.kind(ino)? != mode::S_IFDIR {
        return Err(Errno::ENOTDIR);
    }
    if !real.starts_with(&top) {
        return Err(Errno::EACCES);
    }
    Ok(ino)
}

/// 要删除、改名、移走的目录项：所在的目录在 `top` 里面，名字本身不跟随。`top` 自己不行。
fn entry_in(store: &Store, top: &[u8], path: &[u8]) -> Result<(Ino, Vec<u8>), Errno> {
    let comps: Vec<Vec<u8>> = components(path).collect();
    let (name, dirs) = comps.split_last().ok_or(Errno::EACCES)?;
    if name == b".." {
        return Err(Errno::EINVAL);
    }
    let dir = dir_in(store, top, &join(dirs))?;
    store.lookup(dir, name)?.ok_or(Errno::ENOENT)?;
    Ok((dir, name.clone()))
}

/// 新的名字：不能是空的、`.`、`..`，不能有 `/` 和 NUL，最多 255 字节。
fn check_name(name: &[u8]) -> Result<(), Errno> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(Errno::EINVAL);
    }
    if name.len() > NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    Ok(())
}

/// 名字的最大长度，与 Linux 的 `NAME_MAX` 相同。
const NAME_MAX: usize = 255;

/// 名字已经有了时换的名字：文件是 `a (1).txt`（扩展名留在最后），目录是 `a (1)`。
fn numbered(name: &[u8], n: u32, directory: bool) -> Vec<u8> {
    let dot = name
        .iter()
        .rposition(|&b| b == b'.')
        .filter(|&i| i > 0 && !directory);
    let (stem, ext) = name.split_at(dot.unwrap_or(name.len()));
    let mut out = stem.to_vec();
    out.extend_from_slice(format!(" ({n})").as_bytes());
    out.extend_from_slice(ext);
    out
}

/// 在 `top` 里面的目录 `dir` 里新建文件（0644）或目录（0755），属主是 `uid`、`gid`。名字
/// 已经有了时换一个（[`numbered`]），返回实际用的名字。
pub fn create(
    store: &Store,
    top: &[u8],
    dir: &[u8],
    name: &[u8],
    directory: bool,
    (uid, gid): (u32, u32),
) -> Result<Vec<u8>, Errno> {
    check_name(name)?;
    let dir = dir_in(store, top, dir)?;
    for n in 0..=MAX_TRIES {
        let name = if n == 0 {
            name.to_vec()
        } else {
            numbered(name, n, directory)
        };
        check_name(&name)?;
        let r = if directory {
            store.mkdir(dir, &name, 0o755, uid, gid).map(drop)
        } else {
            store.create_file(dir, &name, 0o644, uid, gid).map(drop)
        };
        match r {
            Ok(()) => return Ok(name),
            Err(Errno::EEXIST) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Errno::EEXIST)
}

/// 删除 `top` 里面的文件、符号链接或者整个目录树（符号链接只删链接）。
pub fn remove(store: &Store, top: &[u8], path: &[u8]) -> Result<(), Errno> {
    let (dir, name) = entry_in(store, top, path)?;
    remove_tree(store, dir, &name)
}

fn remove_tree(store: &Store, dir: Ino, name: &[u8]) -> Result<(), Errno> {
    let ino = store.lookup(dir, name)?.ok_or(Errno::ENOENT)?;
    if store.kind(ino)? != mode::S_IFDIR {
        return store.unlink(dir, name);
    }
    for e in store.readdir(ino)? {
        remove_tree(store, ino, &e.name)?;
    }
    store.rmdir(dir, name)
}

/// 在同一个目录里改名；新的名字已经有了是 EEXIST，不覆盖。
pub fn rename(store: &Store, top: &[u8], path: &[u8], new_name: &[u8]) -> Result<(), Errno> {
    check_name(new_name)?;
    let (dir, name) = entry_in(store, top, path)?;
    if name == new_name {
        return Ok(());
    }
    store.rename(dir, &name, dir, new_name, true, false)
}

/// 移到 `top` 里面的另一个目录 `to`，名字不变；那里已经有了是 EEXIST，目录移进自己里面是
/// EINVAL。
pub fn move_to(store: &Store, top: &[u8], path: &[u8], to: &[u8]) -> Result<(), Errno> {
    let (dir, name) = entry_in(store, top, path)?;
    let to = dir_in(store, top, to)?;
    store.rename(dir, &name, to, &name, true, false)
}

/// 打开 `top` 里面的普通文件读写（符号链接跟随，指向的文件也要在 `top` 里面），`truncate`
/// 时先截断，`append` 时以 `O_APPEND` 打开。返回宿主上的 object。
pub fn open_write(
    store: &Store,
    top: &[u8],
    path: &[u8],
    truncate: bool,
    append: bool,
) -> Result<File, Errno> {
    let top = top_of(store, top)?;
    let (ino, real) = walk(store, path)?;
    if !real.starts_with(&top) {
        return Err(Errno::EACCES);
    }
    match store.kind(ino)? {
        mode::S_IFREG => {}
        mode::S_IFDIR => return Err(Errno::EISDIR),
        _ => return Err(Errno::ENOENT),
    }
    let file = store.open_object_append(ino, append)?;
    if truncate {
        store.truncate(ino, 0)?;
    }
    Ok(file)
}

/// 组件拼成绝对路径。
fn join(comps: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in comps {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// guest 的路径 → 宿主（Java）一侧的字符串（App 的文档 ID）：合法 UTF-8 的部分照原样，`%`、
/// 控制字符和不是 UTF-8 的字节写成 `%XX`，能用 [`decode_path`] 原样换回来，里面也没有制表符、
/// 换行（JNI 那边一项一行、用制表符分隔）。
pub fn encode_path(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '%' || c.is_control() {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    let _ = write!(out, "%{b:02X}");
                }
            } else {
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// [`encode_path`] 反过来；`%` 后面不是两位十六进制数时照原样。
pub fn decode_path(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// 给宿主看的只有目录和普通文件。
fn shown(st: &Stat) -> bool {
    matches!(st.kind(), mode::S_IFDIR | mode::S_IFREG)
}

/// 路径的组件，去掉空的和 `.`。
fn components(path: &[u8]) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn sample(name: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("appine-browse-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::create(&dir).unwrap();
        let home = s.mkdir(ROOT, b"home", 0o755, 0, 0).unwrap();
        let alice = s.mkdir(home, b"alice", 0o700, 1000, 1000).unwrap();
        let docs = s.mkdir(alice, b"Docs", 0o755, 1000, 1000).unwrap();
        let etc = s.mkdir(ROOT, b"etc", 0o755, 0, 0).unwrap();
        s.create_file(etc, b"motd", 0o644, 0, 0)
            .unwrap()
            .1
            .write_all(b"welcome\n")
            .unwrap();
        s.create_file(docs, b"Notes.txt", 0o644, 1000, 1000)
            .unwrap()
            .1
            .write_all(b"hello")
            .unwrap();
        s.symlink(alice, b"motd", b"/etc/motd", 1000, 1000).unwrap();
        s.symlink(alice, b"up", b"..", 1000, 1000).unwrap();
        s.symlink(alice, b"rel", b"Docs/../Docs/Notes.txt", 1000, 1000)
            .unwrap();
        s.symlink(alice, b"broken", b"/nowhere", 1000, 1000)
            .unwrap();
        s.symlink(alice, b"loop", b"loop", 1000, 1000).unwrap();
        s.mknod(alice, b"fifo", mode::S_IFIFO | 0o644, 0, 1000, 1000)
            .unwrap();
        (s, dir)
    }

    #[test]
    fn paths_resolve_through_links() {
        let (s, dir) = sample("resolve");
        let notes = |p: &[u8]| {
            let mut text = String::new();
            open(&s, p).unwrap().read_to_string(&mut text).unwrap();
            text
        };
        assert_eq!(notes(b"/home/alice/Docs/Notes.txt"), "hello");
        assert_eq!(notes(b"/home/alice/rel"), "hello");
        assert_eq!(notes(b"/home/alice/motd"), "welcome\n");
        assert_eq!(notes(b"/../home/./alice//Docs/Notes.txt"), "hello");
        assert!(stat(&s, b"/home/alice/up").unwrap().is_dir());
        assert_eq!(stat(&s, b"/home/alice/broken").unwrap_err(), Errno::ENOENT);
        assert_eq!(stat(&s, b"/home/alice/loop").unwrap_err(), Errno::ELOOP);
        assert_eq!(stat(&s, b"/home/alice/fifo").unwrap_err(), Errno::ENOENT);
        assert_eq!(open(&s, b"/home/alice").unwrap_err(), Errno::EISDIR);
        assert_eq!(
            stat(&s, b"/etc/motd/x").unwrap_err(),
            Errno::ENOTDIR,
            "a file in the middle of the path"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listings_follow_links_and_hide_special_files() {
        let (s, dir) = sample("list");
        let names: Vec<_> = list(&s, b"/home/alice")
            .unwrap()
            .into_iter()
            .map(|e| (String::from_utf8(e.name).unwrap(), e.stat.kind()))
            .collect();
        assert_eq!(
            names,
            [
                ("Docs".to_owned(), mode::S_IFDIR),
                ("motd".to_owned(), mode::S_IFREG),
                ("rel".to_owned(), mode::S_IFREG),
                ("up".to_owned(), mode::S_IFDIR),
            ]
        );
        assert_eq!(list(&s, b"/etc/motd").unwrap_err(), Errno::ENOTDIR);
        let found: Vec<_> = search(&s, b"/home", "NOTES", 10, 1000)
            .unwrap()
            .into_iter()
            .map(|(p, _)| String::from_utf8(p).unwrap())
            .collect();
        assert_eq!(found, ["/home/alice/Docs/Notes.txt"]);
        // 浅的在前，到了上限就停
        let found = search(&s, b"/", "o", 2, 1000).unwrap();
        let paths: Vec<_> = found.iter().map(|(p, _)| p.as_slice()).collect();
        assert_eq!(paths, [b"/home".as_slice(), b"/etc/motd"]);
        assert!(search(&s, b"/", "notes", 10, 3).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paths_survive_the_round_trip() {
        for p in [
            b"/home/alice/Notes.txt".as_slice(),
            "/home/alice/中文 名字".as_bytes(),
            b"/tmp/100%",
            b"/tmp/tab\there\nnew line",
            b"/tmp/\xff\xfe not utf-8 \xe4\xb8",
            b"/tmp/%41",
        ] {
            let s = encode_path(p);
            assert!(!s.contains(['\t', '\n']), "{s}");
            assert_eq!(decode_path(&s), p, "{s}");
        }
        assert_eq!(encode_path(b"/a%b\tc"), "/a%25b%09c");
        assert_eq!(encode_path("中".as_bytes()), "中");
        assert_eq!(decode_path("/x%zz%4"), b"/x%zz%4");
    }

    #[test]
    fn changes_stay_inside_the_top_directory() {
        let (s, dir) = sample("write");
        let top = b"/home/alice";
        let owner = (1000, 1000);
        let read = |p: &[u8]| {
            let mut text = String::new();
            open(&s, p).unwrap().read_to_string(&mut text).unwrap();
            text
        };
        // 名字已经有了时换一个；扩展名留在最后，点开头的名字没有扩展名
        assert_eq!(
            create(&s, top, top, b"a.txt", false, owner).unwrap(),
            b"a.txt"
        );
        assert_eq!(
            create(&s, top, top, b"a.txt", false, owner).unwrap(),
            b"a (1).txt"
        );
        assert_eq!(
            create(&s, top, top, b"Docs", true, owner).unwrap(),
            b"Docs (1)"
        );
        assert_eq!(create(&s, top, top, b".rc", false, owner).unwrap(), b".rc");
        assert_eq!(
            create(&s, top, top, b".rc", false, owner).unwrap(),
            b".rc (1)"
        );
        for bad in [b"".as_slice(), b"..", b"x/y", b"nul\0"] {
            assert_eq!(
                create(&s, top, top, bad, false, owner).unwrap_err(),
                Errno::EINVAL
            );
        }
        let st = stat(&s, b"/home/alice/a.txt").unwrap();
        assert_eq!((st.uid, st.gid, st.mode & 0o7777), (1000, 1000, 0o644));
        let st = stat(&s, b"/home/alice/Docs (1)").unwrap();
        assert_eq!((st.uid, st.mode & 0o7777), (1000, 0o755));
        // 写；截断之后再写
        open_write(&s, top, b"/home/alice/a.txt", false, false)
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        // 追加：写在末尾
        open_write(&s, top, b"/home/alice/a.txt", false, true)
            .unwrap()
            .write_all(b"!")
            .unwrap();
        assert_eq!(read(b"/home/alice/a.txt"), "hello!");
        open_write(&s, top, b"/home/alice/a.txt", true, false)
            .unwrap()
            .write_all(b"hi")
            .unwrap();
        assert_eq!(read(b"/home/alice/a.txt"), "hi");
        // 家目录外面的、经由符号链接走出去的都不行
        assert_eq!(
            create(&s, top, b"/etc", b"x", false, owner).unwrap_err(),
            Errno::EACCES
        );
        let motd = b"/home/alice/motd";
        assert_eq!(
            open_write(&s, top, motd, true, false).unwrap_err(),
            Errno::EACCES
        );
        assert_eq!(read(b"/etc/motd"), "welcome\n");
        assert_eq!(
            remove(&s, top, b"/home/alice/up/alice").unwrap_err(),
            Errno::EACCES
        );
        assert_eq!(remove(&s, top, top).unwrap_err(), Errno::EACCES);
        assert_eq!(
            move_to(&s, top, b"/home/alice/a.txt", b"/etc").unwrap_err(),
            Errno::EACCES
        );
        // 删除符号链接只删链接
        remove(&s, top, motd).unwrap();
        assert_eq!(stat(&s, motd).unwrap_err(), Errno::ENOENT);
        assert_eq!(read(b"/etc/motd"), "welcome\n");
        // 改名不覆盖
        let a1 = b"/home/alice/a (1).txt";
        assert_eq!(rename(&s, top, a1, b"a.txt").unwrap_err(), Errno::EEXIST);
        assert_eq!(rename(&s, top, a1, b"x/y").unwrap_err(), Errno::EINVAL);
        rename(&s, top, a1, b"b.txt").unwrap();
        assert!(stat(&s, b"/home/alice/b.txt").is_ok());
        // 移动；目录不能移进自己里面
        move_to(&s, top, b"/home/alice/b.txt", b"/home/alice/Docs").unwrap();
        assert!(stat(&s, b"/home/alice/Docs/b.txt").is_ok());
        assert_eq!(
            move_to(&s, top, b"/home/alice/Docs", b"/home/alice/Docs").unwrap_err(),
            Errno::EINVAL
        );
        // 删掉整个目录树
        remove(&s, top, b"/home/alice/Docs").unwrap();
        assert_eq!(stat(&s, b"/home/alice/Docs").unwrap_err(), Errno::ENOENT);
        remove(&s, top, b"/home/alice/fifo").unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
