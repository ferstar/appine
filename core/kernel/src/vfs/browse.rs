//! 宿主浏览仓库（App 的 DocumentsProvider，设计文档 §11.3）：不经过内核，直接在仓库里按
//! guest 路径查找、列目录、打开文件。
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
    // 还没走的组件，倒着放（栈顶是下一个）；跟随符号链接时把目标的组件压进去
    let mut pending: Vec<Vec<u8>> = components(path).rev().collect();
    let mut dir = ROOT;
    let mut links = 0;
    while let Some(name) = pending.pop() {
        if store.kind(dir)? != mode::S_IFDIR {
            return Err(Errno::ENOTDIR);
        }
        if name == b".." {
            if dir != ROOT {
                dir = store.parent(dir)?;
            }
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
                dir = ROOT;
            }
            pending.extend(components(&target).rev());
            continue;
        }
        dir = ino;
    }
    Ok(dir)
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
}
