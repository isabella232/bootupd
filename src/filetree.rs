/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use anyhow::{bail, Context, Result};
use byteorder::ByteOrder;
use chrono::prelude::*;
use openat_ext::OpenatDirExt;
use openssl::hash::{Hasher, MessageDigest};
use serde_derive::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::linux::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
// Seems like a rust-analyzer bug?
#[allow(unused_imports)]
use std::io::Write;

/// The prefix we apply to our temporary files.
pub(crate) const TMP_PREFIX: &'static str = ".btmp.";

use crate::sha512string::SHA512String;

/// Metadata for a single file
#[derive(Serialize, Debug, Hash, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct FileMetadata {
    /// File size in bytes
    pub(crate) size: u64,
    /// Content checksum; chose SHA-512 because there are not a lot of files here
    /// and it's ok if the checksum is large.
    pub(crate) sha512: SHA512String,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct FileTree {
    pub(crate) timestamp: NaiveDateTime,
    pub(crate) children: BTreeMap<String, FileMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FileTreeDiff {
    pub(crate) additions: HashSet<String>,
    pub(crate) removals: HashSet<String>,
    pub(crate) changes: HashSet<String>,
}

impl FileTreeDiff {
    #[allow(dead_code)] // Used for testing
    pub(crate) fn count(&self) -> usize {
        self.additions.len() + self.removals.len() + self.changes.len()
    }
}

impl FileMetadata {
    pub(crate) fn new_from_path<P: openat::AsPath>(
        dir: &openat::Dir,
        name: P,
    ) -> Result<FileMetadata> {
        let mut r = dir.open_file(name)?;
        let meta = r.metadata()?;
        let mut hasher =
            Hasher::new(MessageDigest::sha512()).expect("openssl sha512 hasher creation failed");
        std::io::copy(&mut r, &mut hasher)?;
        let digest = SHA512String::from_hasher(&mut hasher);
        Ok(FileMetadata {
            size: meta.len(),
            sha512: digest,
        })
    }

    pub(crate) fn extend_hash(&self, hasher: &mut Hasher) {
        let mut lenbuf = [0; 8];
        byteorder::BigEndian::write_u64(&mut lenbuf, self.size);
        hasher.update(&lenbuf).unwrap();
        hasher.update(&self.sha512.digest_bytes()).unwrap();
    }
}

impl FileTree {
    // Internal helper to generate a sub-tree
    fn unsorted_from_dir(dir: &openat::Dir) -> Result<HashMap<String, FileMetadata>> {
        let mut ret = HashMap::new();
        for entry in dir.list_dir(".")? {
            let entry = entry?;
            let name = if let Some(name) = entry.file_name().to_str() {
                name
            } else {
                bail!("Invalid UTF-8 filename: {:?}", entry.file_name())
            };
            if name.starts_with(TMP_PREFIX) {
                bail!("File {} contains our temporary prefix!", name);
            }
            match dir.get_file_type(&entry)? {
                openat::SimpleType::File => {
                    let meta = FileMetadata::new_from_path(dir, name)?;
                    ret.insert(name.to_string(), meta);
                }
                openat::SimpleType::Dir => {
                    let child = dir.sub_dir(name)?;
                    for (mut k, v) in FileTree::unsorted_from_dir(&child)?.drain() {
                        k.reserve(name.len() + 1);
                        k.insert(0, '/');
                        k.insert_str(0, name);
                        ret.insert(k, v);
                    }
                }
                openat::SimpleType::Symlink => {
                    bail!("Unsupported symbolic link {:?}", entry.file_name())
                }
                openat::SimpleType::Other => {
                    bail!("Unsupported non-file/directory {:?}", entry.file_name())
                }
            }
        }
        Ok(ret)
    }

    pub(crate) fn digest(&self) -> SHA512String {
        let mut hasher =
            Hasher::new(MessageDigest::sha512()).expect("openssl sha512 hasher creation failed");
        for (k, v) in self.children.iter() {
            hasher.update(k.as_bytes()).unwrap();
            v.extend_hash(&mut hasher);
        }
        SHA512String::from_hasher(&mut hasher)
    }

    /// Create a FileTree from the target directory.
    pub(crate) fn new_from_dir(dir: &openat::Dir) -> Result<Self> {
        let mut children = BTreeMap::new();
        for (k, v) in Self::unsorted_from_dir(dir)?.drain() {
            children.insert(k, v);
        }

        let meta = dir.metadata(".")?;
        let stat = meta.stat();

        Ok(Self {
            timestamp: chrono::NaiveDateTime::from_timestamp(stat.st_mtime, 0),
            children: children,
        })
    }

    /// Determine the changes *from* self to the updated tree
    pub(crate) fn diff(&self, updated: &Self) -> Result<FileTreeDiff> {
        let mut additions = HashSet::new();
        let mut removals = HashSet::new();
        let mut changes = HashSet::new();

        for (k, v1) in self.children.iter() {
            if let Some(v2) = updated.children.get(k) {
                if v1 != v2 {
                    changes.insert(k.clone());
                }
            } else {
                removals.insert(k.clone());
            }
        }
        for k in updated.children.keys() {
            if let Some(_) = self.children.get(k) {
                continue;
            }
            additions.insert(k.clone());
        }
        Ok(FileTreeDiff {
            additions: additions,
            removals: removals,
            changes: changes,
        })
    }
}

// Recursively remove all files in the directory that start with our TMP_PREFIX
fn cleanup_tmp(dir: &openat::Dir) -> Result<()> {
    for entry in dir.list_dir(".")? {
        let entry = entry?;
        let name = if let Some(name) = entry.file_name().to_str() {
            name
        } else {
            // Skip invalid UTF-8 for now, we will barf on it later though.
            continue;
        };

        match dir.get_file_type(&entry)? {
            openat::SimpleType::Dir => {
                let child = dir.sub_dir(name)?;
                cleanup_tmp(&child)?;
            }
            openat::SimpleType::File => {
                if name.starts_with(TMP_PREFIX) {
                    dir.remove_file(name)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Default, Clone)]
pub(crate) struct ApplyUpdateOptions {
    pub(crate) skip_removals: bool,
    pub(crate) skip_sync: bool,
}

/// A bit like std::fs::copy but operates dirfd-relative
fn copy_file_at<SP: AsRef<Path>, DP: AsRef<Path>>(
    srcdir: &openat::Dir,
    destdir: &openat::Dir,
    srcp: SP,
    destp: DP,
) -> Result<()> {
    let srcp = srcp.as_ref();
    let srcf = srcdir.open_file(srcp)?;
    let srcmeta = srcf.metadata()?;
    let mode = srcmeta.st_mode();
    let mut srcf = std::io::BufReader::new(srcf);
    let mut destf = destdir.write_file(destp.as_ref(), mode)?;
    {
        let mut destf_buf = std::io::BufWriter::new(&mut destf);
        let _ = std::io::copy(&mut srcf, &mut destf_buf)?;
        destf_buf.flush()?;
    }
    destf.flush()?;

    Ok(())
}

// syncfs() is a Linux-specific system call, which doesn't seem
// to be bound in nix today.  I found https://github.com/XuShaohua/nc
// but that's a nontrivial dependency with not a lot of code review.
// Let's just fork off a helper process for now.
pub(crate) fn syncfs(d: &openat::Dir) -> Result<()> {
    let d = d.sub_dir(".").expect("subdir");
    let mut c = std::process::Command::new("sync");
    let c = c.args(&["-f", "."]);
    unsafe {
        c.pre_exec(move || {
            nix::unistd::fchdir(d.as_raw_fd()).expect("fchdir");
            Ok(())
        })
    };
    let r = c.status().context("syncfs failed")?;
    if !r.success() {
        bail!("syncfs failed");
    }
    Ok(())
}

fn tmpname_for_path<P: AsRef<Path>>(path: P) -> std::path::PathBuf {
    let path = path.as_ref();
    let basename = path.file_name().expect("filename");
    path.with_file_name(format!(
        "{}{}",
        TMP_PREFIX,
        basename.to_str().expect("UTF-8 filename")
    ))
}

/// Given two directories, apply a diff generated from srcdir to destdir
pub(crate) fn apply_diff(
    srcdir: &openat::Dir,
    destdir: &openat::Dir,
    diff: &FileTreeDiff,
    opts: Option<&ApplyUpdateOptions>,
) -> Result<()> {
    let default_opts = ApplyUpdateOptions {
        ..Default::default()
    };
    let opts = opts.unwrap_or(&default_opts);
    cleanup_tmp(destdir).context("cleaning up temporary files")?;

    // Write new and changed files
    for pathstr in diff.additions.iter().chain(diff.changes.iter()) {
        let path = Path::new(pathstr);
        if let Some(parent) = path.parent() {
            // TODO: care about directory modes?  Eh.
            std::fs::create_dir_all(parent)?;
        }
        let destp = tmpname_for_path(path);
        copy_file_at(srcdir, destdir, path, destp.as_path())
            .with_context(|| format!("writing {}", &pathstr))?;
    }
    // Ensure all of the new files are written persistently to disk
    if !opts.skip_sync {
        syncfs(destdir)?;
    }
    // Now move them all into place (TODO track interruption)
    for path in diff.additions.iter().chain(diff.changes.iter()) {
        let pathtmp = tmpname_for_path(path);
        destdir
            .local_rename(&pathtmp, path)
            .with_context(|| format!("renaming {}", path))?;
    }
    if !opts.skip_removals {
        for path in diff.removals.iter() {
            destdir
                .remove_file(path)
                .with_context(|| format!("removing {}", path))?;
        }
    }
    // A second full filesystem sync to narrow any races rather than
    // waiting for writeback to kick in.
    if !opts.skip_sync {
        syncfs(destdir)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_diff(a: &openat::Dir, b: &openat::Dir) -> Result<FileTreeDiff> {
        let ta = FileTree::new_from_dir(&a)?;
        let tb = FileTree::new_from_dir(&b)?;
        let diff = ta.diff(&tb)?;
        Ok(diff)
    }

    fn test_one_apply<AP: AsRef<Path>, BP: AsRef<Path>>(
        a: AP,
        b: BP,
        opts: Option<&ApplyUpdateOptions>,
    ) -> Result<()> {
        let a = a.as_ref();
        let b = b.as_ref();
        let t = tempfile::tempdir()?;
        let c = t.path().join("c");
        let r = std::process::Command::new("cp")
            .arg("-rp")
            .args(&[a, &c])
            .status()?;
        if !r.success() {
            bail!("failed to cp");
        };
        let c = openat::Dir::open(&c)?;
        let da = openat::Dir::open(a)?;
        let db = openat::Dir::open(b)?;
        let ta = FileTree::new_from_dir(&da)?;
        let tb = FileTree::new_from_dir(&db)?;
        let diff = ta.diff(&tb)?;
        let rdiff = tb.diff(&ta)?;
        assert_eq!(diff.count(), rdiff.count());
        assert_eq!(diff.additions.len(), rdiff.removals.len());
        assert_eq!(diff.changes.len(), rdiff.changes.len());
        apply_diff(&db, &c, &diff, opts)?;
        let tc = FileTree::new_from_dir(&c)?;
        let newdiff = tb.diff(&tc)?;
        let skip_removals = opts.map(|o| o.skip_removals).unwrap_or(false);
        if skip_removals {
            let n = newdiff.count();
            if n != 0 {
                assert_eq!(n, diff.removals.len());
            }
            for f in diff.removals.iter() {
                assert!(c.exists(f)?);
                assert!(da.exists(f)?);
            }
        } else {
            assert_eq!(newdiff.count(), 0);
        }
        Ok(())
    }

    fn test_apply<AP: AsRef<Path>, BP: AsRef<Path>>(a: AP, b: BP) -> Result<()> {
        let a = a.as_ref();
        let b = b.as_ref();
        let skip_removals = ApplyUpdateOptions {
            skip_removals: true,
            ..Default::default()
        };
        test_one_apply(&a, &b, None).context("testing apply (with removals)")?;
        test_one_apply(&a, &b, Some(&skip_removals))
            .context("testing apply (skipping removals)")?;
        Ok(())
    }

    #[test]
    fn test_filetree() -> Result<()> {
        let tmpd = tempfile::tempdir()?;
        let p = tmpd.path();
        let pa = p.join("a");
        let pb = p.join("b");
        std::fs::create_dir(&pa)?;
        std::fs::create_dir(&pb)?;
        let a = openat::Dir::open(&pa)?;
        let b = openat::Dir::open(&pb)?;
        let diff = run_diff(&a, &b)?;
        assert_eq!(diff.count(), 0);
        a.create_dir("foo", 0o755)?;
        let diff = run_diff(&a, &b)?;
        assert_eq!(diff.count(), 0);
        {
            let mut bar = a.write_file("foo/bar", 0o644)?;
            bar.write("foobarcontents".as_bytes())?;
        }
        let diff = run_diff(&a, &b)?;
        assert_eq!(diff.count(), 1);
        assert_eq!(diff.removals.len(), 1);
        test_apply(&pa, &pb).context("testing apply 1")?;

        b.create_dir("foo", 0o755)?;
        {
            let mut bar = b.write_file("foo/bar", 0o644)?;
            bar.write("foobarcontents".as_bytes())?;
        }
        let diff = run_diff(&a, &b)?;
        assert_eq!(diff.count(), 0);
        test_apply(&pa, &pb).context("testing apply 2")?;
        {
            let mut bar2 = b.write_file("foo/bar", 0o644)?;
            bar2.write("foobarcontents2".as_bytes())?;
        }
        let diff = run_diff(&a, &b)?;
        assert_eq!(diff.count(), 1);
        assert_eq!(diff.changes.len(), 1);
        test_apply(&pa, &pb).context("testing apply 3")?;
        Ok(())
    }
}
