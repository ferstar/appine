# Extended attributes (kernel/xattr) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/xattr"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Extended-attribute decisions made before kernel/xattr was split from kernel/vfs keep their `kernel/vfs` tags and live in `.agents/decisions/kernel/vfs.md`: D38 and D50.

## Active

### D1: kernel/xattr is split out of kernel/vfs
- **What**: kernel/xattr owns `syscall/xattr.rs`: name prefixes and permissions, POSIX ACLs stored but not enforced and `security.capability`'s checks and conversions, while where values live is kernel/store's, as the file's doc comment now says.
- **Why**: `syscall/xattr.rs` is self-contained and called only from the dispatcher, and the vfs map was still 49.2 KB after pipes left.
- **Tradeoffs**: Overlay and FUSE xattrs are described in their own maps.
- **Watch out**: The map keeps open the unenforced-ACL bug and the question of `ENODATA` on synthetic nodes.
- **Source**: 5f327de

### D2: ACL, mode and timestamp changes form one visible metadata update

- **What**: Commit ACL and mode together, enforce named entries, preserve legacy class permissions from the inode, and notify successful FUSE xattr mutations.
- **Why**: A stored ACL must agree with permission checks and reflected timestamps.
- **Tradeoffs**: Updating held or copied-up files can require store metadata work.
- **Watch out**: Retain named entries while migrating legacy permissions.
- **Source**: ab694aab, 231480cb, aeaffd4b, 68b70897

### D3: Synthetic xattr queries follow filesystem class and permission order

- **What**: Reject system names as unsupported, check access, then distinguish missing kernfs attributes from unsupported procfs, devpts, nsfs and mqueue operations via fs_of.
- **Why**: Different synthetic filesystems expose different xattr interfaces.
- **Tradeoffs**: The result depends on both namespace and filesystem kind.
- **Watch out**: Keep classification tied to the common filesystem identity.
- **Source**: e6495409, bdcaddad

## Superseded

- ~~Storing named ACL entries without enforcing them (kernel/vfs D50)~~ → replaced by **D2** in 68b70897 (2026-11-24)
