//! File transfer: read or write a file on the runner, with byte-offset support
//! for chunked, resumable transfer of files larger than one protocol frame
//! (32 MiB). The controller drives chunking by looping over offsets.

use arc_proto::wire::{FileHash, Reply};
use blake2::{Blake2s256, Digest};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::dispatch::{RemoteResult, not_found, os_error};

/// Directories never enumerated by [`list_tree`] (build outputs / VCS) — must
/// match the controller's sync skip-list so `--delete` can never prune them.
const SKIP_DIRS: &[&str] = &["target", "bin", "obj", "node_modules", ".git"];

/// Lists file paths (relative to `root`, forward-slash) recursively, skipping
/// [`SKIP_DIRS`]. Missing root yields an empty listing.
pub async fn list_tree(root: &str) -> RemoteResult<Reply> {
    let base = std::path::Path::new(root);
    let mut files = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                let name = entry.file_name();
                if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file()
                && let Ok(rel) = path.strip_prefix(base)
            {
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(Reply::Tree(files))
}

/// Deletes a file; a missing file is treated as already-deleted (success).
pub async fn delete_file(path: &str) -> RemoteResult<Reply> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(Reply::Ack),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Reply::Ack),
        Err(e) => Err(os_error(format!("delete {path}: {e}"))),
    }
}

/// Content-hashes each `root`-relative path for sync diffing; a missing file
/// hashes to `None` (so the controller knows to push it).
pub async fn hash_files(root: &str, paths: &[String]) -> RemoteResult<Reply> {
    let base = std::path::Path::new(root);
    let mut hashes = Vec::with_capacity(paths.len());
    for rel in paths {
        let hash = match tokio::fs::read(base.join(rel)).await {
            Ok(bytes) => {
                let mut hasher = Blake2s256::new();
                hasher.update(&bytes);
                Some(hex(&hasher.finalize()))
            }
            Err(_) => None,
        };
        hashes.push(FileHash {
            path: rel.clone(),
            hash,
        });
    }
    Ok(Reply::FileHashes(hashes))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Reads up to `max_len` bytes (whole file if `max_len == 0`) from `offset`.
pub async fn read_file(path: &str, offset: u64, max_len: u64) -> RemoteResult<Reply> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_found(format!("{path}: not found")));
        }
        Err(e) => return Err(os_error(format!("open {path}: {e}"))),
    };
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| os_error(format!("seek {path}: {e}")))?;
    }

    let mut buffer = Vec::new();
    let read_result = if max_len == 0 {
        file.read_to_end(&mut buffer).await.map(|_| ())
    } else {
        // Read up to max_len, tolerating short reads from the OS.
        buffer.resize(max_len as usize, 0);
        let mut filled = 0usize;
        loop {
            match file.read(&mut buffer[filled..]).await {
                Ok(0) => break,
                Ok(n) => {
                    filled += n;
                    if filled == buffer.len() {
                        break;
                    }
                }
                Err(e) => {
                    return Err(os_error(format!("read {path}: {e}")));
                }
            }
        }
        buffer.truncate(filled);
        Ok(())
    };
    read_result.map_err(|e| os_error(format!("read {path}: {e}")))?;

    Ok(Reply::FileContents(buffer))
}

/// Writes `contents` at `offset`. `offset == 0` creates/truncates the file;
/// `offset > 0` opens it for writing and seeks there. Parent directories are
/// created as needed.
pub async fn write_file(path: &str, contents: &[u8], offset: u64) -> RemoteResult<Reply> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| os_error(format!("create {}: {e}", parent.display())))?;
    }

    let mut file = if offset == 0 {
        tokio::fs::File::create(path)
            .await
            .map_err(|e| os_error(format!("create {path}: {e}")))?
    } else {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false) // keep existing bytes; we seek and overwrite at offset
            .open(path)
            .await
            .map_err(|e| os_error(format!("open {path}: {e}")))?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| os_error(format!("seek {path}: {e}")))?;
        file
    };

    file.write_all(contents)
        .await
        .map_err(|e| os_error(format!("write {path}: {e}")))?;
    file.flush()
        .await
        .map_err(|e| os_error(format!("flush {path}: {e}")))?;
    Ok(Reply::Ack)
}
