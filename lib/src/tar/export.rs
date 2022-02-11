//! APIs for creating container images from OSTree commits

use crate::objgv::*;
use anyhow::{anyhow, bail, ensure, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fn_error_context::context;
use gio::glib;
use gio::prelude::*;
use gvariant::aligned_bytes::TryAsAligned;
use gvariant::{Marker, Structure};
use ostree::gio;
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::BufReader;

/// The repository mode generated by a tar export stream.
pub const BARE_SPLIT_XATTRS_MODE: &str = "bare-split-xattrs";

// This is both special in the tar stream *and* it's in the ostree commit.
const SYSROOT: &str = "sysroot";
// This way the default ostree -> sysroot/ostree symlink works.
const OSTREEDIR: &str = "sysroot/ostree";

/// The base repository configuration that identifies this is a tar export.
// See https://github.com/ostreedev/ostree/issues/2499
const REPO_CONFIG: &str = r#"[core]
repo_version=1
mode=bare-split-xattrs
"#;

/// A decently large buffer, as used by e.g. coreutils `cat`.
/// System calls are expensive.
const BUF_CAPACITY: usize = 131072;

/// Convert /usr/etc back to /etc
fn map_path(p: &Utf8Path) -> std::borrow::Cow<Utf8Path> {
    match p.strip_prefix("./usr/etc") {
        Ok(r) => Cow::Owned(Utf8Path::new("./etc").join(r)),
        _ => Cow::Borrowed(p),
    }
}

struct OstreeTarWriter<'a, W: std::io::Write> {
    repo: &'a ostree::Repo,
    out: &'a mut tar::Builder<W>,
    options: ExportOptions,
    wrote_initdirs: bool,
    wrote_dirtree: HashSet<String>,
    wrote_dirmeta: HashSet<String>,
    wrote_content: HashSet<String>,
    wrote_xattrs: HashSet<String>,
}

fn object_path(objtype: ostree::ObjectType, checksum: &str) -> Utf8PathBuf {
    let suffix = match objtype {
        ostree::ObjectType::Commit => "commit",
        ostree::ObjectType::CommitMeta => "commitmeta",
        ostree::ObjectType::DirTree => "dirtree",
        ostree::ObjectType::DirMeta => "dirmeta",
        ostree::ObjectType::File => "file",
        o => panic!("Unexpected object type: {:?}", o),
    };
    let (first, rest) = checksum.split_at(2);
    format!("{}/repo/objects/{}/{}.{}", OSTREEDIR, first, rest, suffix).into()
}

fn v0_xattrs_path(checksum: &str) -> Utf8PathBuf {
    format!("{}/repo/xattrs/{}", OSTREEDIR, checksum).into()
}

fn v0_xattrs_object_path(checksum: &str) -> Utf8PathBuf {
    let (first, rest) = checksum.split_at(2);
    format!("{}/repo/objects/{}/{}.file.xattrs", OSTREEDIR, first, rest).into()
}

fn v1_xattrs_object_path(checksum: &str) -> Utf8PathBuf {
    let (first, rest) = checksum.split_at(2);
    format!("{}/repo/objects/{}/{}.file-xattrs", OSTREEDIR, first, rest).into()
}

fn v1_xattrs_link_object_path(checksum: &str) -> Utf8PathBuf {
    let (first, rest) = checksum.split_at(2);
    format!(
        "{}/repo/objects/{}/{}.file-xattrs-link",
        OSTREEDIR, first, rest
    )
    .into()
}

/// Check for "denormal" symlinks which contain "//"
// See https://github.com/fedora-sysv/chkconfig/pull/67
// [root@cosa-devsh ~]# rpm -qf /usr/lib/systemd/systemd-sysv-install
// chkconfig-1.13-2.el8.x86_64
// [root@cosa-devsh ~]# ll /usr/lib/systemd/systemd-sysv-install
// lrwxrwxrwx. 2 root root 24 Nov 29 18:08 /usr/lib/systemd/systemd-sysv-install -> ../../..//sbin/chkconfig
// [root@cosa-devsh ~]#
fn symlink_is_denormal(target: &str) -> bool {
    target.contains("//")
}

impl<'a, W: std::io::Write> OstreeTarWriter<'a, W> {
    fn new(repo: &'a ostree::Repo, out: &'a mut tar::Builder<W>, options: ExportOptions) -> Self {
        Self {
            repo,
            out,
            options,
            wrote_initdirs: false,
            wrote_dirmeta: HashSet::new(),
            wrote_dirtree: HashSet::new(),
            wrote_content: HashSet::new(),
            wrote_xattrs: HashSet::new(),
        }
    }

    /// Convert the ostree mode to tar mode.
    /// The ostree mode bits include the format, tar does not.
    /// Historically in format version 0 we injected them, so we need to keep doing so.
    fn filter_mode(&self, mode: u32) -> u32 {
        if self.options.format_version == 0 {
            mode
        } else {
            mode & !libc::S_IFMT
        }
    }

    /// Add a directory entry with default permissions (root/root 0755)
    fn append_default_dir(&mut self, path: &Utf8Path) -> Result<()> {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(0o755);
        h.set_size(0);
        self.out.append_data(&mut h, &path, &mut std::io::empty())?;
        Ok(())
    }

    /// Add a regular file entry with default permissions (root/root 0644)
    fn append_default_data(&mut self, path: &Utf8Path, data: &[u8]) -> Result<()> {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(0o644);
        h.set_size(data.len() as u64);
        self.out.append_data(&mut h, &path, data)?;
        Ok(())
    }

    /// Add an hardlink entry with default permissions (root/root 0644)
    fn append_default_hardlink(&mut self, path: &Utf8Path, link_target: &Utf8Path) -> Result<()> {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Link);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(0o644);
        h.set_size(0);
        self.out.append_link(&mut h, &path, &link_target)?;
        Ok(())
    }

    /// Write the initial /sysroot/ostree/repo structure.
    fn write_repo_structure(&mut self) -> Result<()> {
        if self.wrote_initdirs {
            return Ok(());
        }

        let objdir: Utf8PathBuf = format!("{}/repo/objects", OSTREEDIR).into();
        // Add all parent directories
        let parent_dirs = {
            let mut parts: Vec<_> = objdir.ancestors().collect();
            parts.reverse();
            parts
        };
        for path in parent_dirs {
            match path.as_str() {
                "/" | "" => continue,
                _ => {}
            }
            self.append_default_dir(path)?;
        }
        // Object subdirectories
        for d in 0..=0xFF {
            let path: Utf8PathBuf = format!("{}/{:02x}", objdir, d).into();
            self.append_default_dir(&path)?;
        }
        // Tmp subdirectories
        for d in ["tmp", "tmp/cache"] {
            let path: Utf8PathBuf = format!("{}/repo/{}", OSTREEDIR, d).into();
            self.append_default_dir(&path)?;
        }
        // Refs subdirectories
        for d in ["refs", "refs/heads", "refs/mirrors", "refs/remotes"] {
            let path: Utf8PathBuf = format!("{}/repo/{}", OSTREEDIR, d).into();
            self.append_default_dir(&path)?;
        }

        // The special `repo/xattrs` directory used in v0 format.
        if self.options.format_version == 0 {
            let path: Utf8PathBuf = format!("{}/repo/xattrs", OSTREEDIR).into();
            self.append_default_dir(&path)?;
        }

        // Repository configuration file.
        {
            let path = match self.options.format_version {
                0 => format!("{}/config", SYSROOT),
                1 => format!("{}/repo/config", OSTREEDIR),
                n => anyhow::bail!("Unsupported ostree tar format version {}", n),
            };
            self.append_default_data(Utf8Path::new(&path), REPO_CONFIG.as_bytes())?;
        }

        self.wrote_initdirs = true;
        Ok(())
    }

    /// Recursively serialize a commit object to the target tar stream.
    fn write_commit(&mut self, checksum: &str) -> Result<()> {
        let cancellable = gio::NONE_CANCELLABLE;

        let (commit_v, _) = self.repo.load_commit(checksum)?;
        let commit_v = &commit_v;

        let commit_bytes = commit_v.data_as_bytes();
        let commit_bytes = commit_bytes.try_as_aligned()?;
        let commit = gv_commit!().cast(commit_bytes);
        let commit = commit.to_tuple();
        let contents = hex::encode(commit.6);
        let metadata_checksum = &hex::encode(commit.7);
        let metadata_v = self
            .repo
            .load_variant(ostree::ObjectType::DirMeta, metadata_checksum)?;
        // Safety: We passed the correct variant type just above
        let metadata = &ostree::DirMetaParsed::from_variant(&metadata_v).unwrap();
        let rootpath = Utf8Path::new("./");

        // We need to write the root directory, before we write any objects.  This should be the very
        // first thing.
        self.append_dir(rootpath, metadata)?;

        // Now, we create sysroot/ and everything under it
        self.write_repo_structure()?;

        self.append(ostree::ObjectType::Commit, checksum, commit_v)?;
        if let Some(commitmeta) = self
            .repo
            .read_commit_detached_metadata(checksum, cancellable)?
        {
            self.append(ostree::ObjectType::CommitMeta, checksum, &commitmeta)?;
        }

        // The ostree dirmeta object for the root.
        self.append(ostree::ObjectType::DirMeta, metadata_checksum, &metadata_v)?;

        // Recurse and write everything else.
        self.append_dirtree(Utf8Path::new("./"), contents, true, cancellable)?;
        Ok(())
    }

    fn append(
        &mut self,
        objtype: ostree::ObjectType,
        checksum: &str,
        v: &glib::Variant,
    ) -> Result<()> {
        let set = match objtype {
            ostree::ObjectType::Commit | ostree::ObjectType::CommitMeta => None,
            ostree::ObjectType::DirTree => Some(&mut self.wrote_dirtree),
            ostree::ObjectType::DirMeta => Some(&mut self.wrote_dirmeta),
            o => panic!("Unexpected object type: {:?}", o),
        };
        if let Some(set) = set {
            if set.contains(checksum) {
                return Ok(());
            }
            let inserted = set.insert(checksum.to_string());
            debug_assert!(inserted);
        }

        let data = v.data_as_bytes();
        let data = data.as_ref();
        self.append_default_data(&object_path(objtype, checksum), data)
            .with_context(|| format!("Writing object {}", checksum))?;
        Ok(())
    }

    /// Export xattrs to the tar stream, return whether content was written.
    #[context("Writing xattrs")]
    fn append_xattrs(&mut self, checksum: &str, xattrs: &glib::Variant) -> Result<bool> {
        let xattrs_data = xattrs.data_as_bytes();
        let xattrs_data = xattrs_data.as_ref();
        if xattrs_data.is_empty() && self.options.format_version == 0 {
            return Ok(false);
        }

        let xattrs_checksum = {
            let digest = openssl::hash::hash(openssl::hash::MessageDigest::sha256(), xattrs_data)?;
            &hex::encode(digest)
        };

        if self.options.format_version == 0 {
            let path = v0_xattrs_path(xattrs_checksum);

            // Write xattrs content into a separate directory.
            if !self.wrote_xattrs.contains(xattrs_checksum) {
                let inserted = self.wrote_xattrs.insert(checksum.to_string());
                debug_assert!(inserted);
                self.append_default_data(&path, xattrs_data)?;
            }
            // Hardlink the object in the repo.
            {
                let objpath = v0_xattrs_object_path(checksum);
                self.append_default_hardlink(&objpath, &path)?;
            }
        } else if self.options.format_version == 1 {
            let path = v1_xattrs_object_path(xattrs_checksum);

            // Write xattrs content into a separate `.file-xattrs` object.
            if !self.wrote_xattrs.contains(xattrs_checksum) {
                let inserted = self.wrote_xattrs.insert(checksum.to_string());
                debug_assert!(inserted);
                self.append_default_data(&path, xattrs_data)?;
            }
            // Write a `.file-xattrs-link` which links the file object to
            // the corresponding detached xattrs.
            {
                let link_obj_path = v1_xattrs_link_object_path(checksum);
                self.append_default_hardlink(&link_obj_path, &path)?;
            }
        } else {
            bail!("Unknown format version '{}'", self.options.format_version);
        }

        Ok(true)
    }

    /// Write a content object, returning the path/header that should be used
    /// as a hard link to it in the target path. This matches how ostree checkouts work.
    fn append_content(&mut self, checksum: &str) -> Result<(Utf8PathBuf, tar::Header)> {
        let path = object_path(ostree::ObjectType::File, checksum);

        let (instream, meta, xattrs) = self.repo.load_file(checksum, gio::NONE_CANCELLABLE)?;
        let meta = meta.ok_or_else(|| anyhow!("Missing metadata for object {}", checksum))?;
        let xattrs = xattrs.ok_or_else(|| anyhow!("Missing xattrs for object {}", checksum))?;

        let mut h = tar::Header::new_gnu();
        h.set_uid(meta.attribute_uint32("unix::uid") as u64);
        h.set_gid(meta.attribute_uint32("unix::gid") as u64);
        let mode = meta.attribute_uint32("unix::mode");
        h.set_mode(self.filter_mode(mode));
        let mut target_header = h.clone();
        target_header.set_size(0);

        if !self.wrote_content.contains(checksum) {
            let inserted = self.wrote_content.insert(checksum.to_string());
            debug_assert!(inserted);

            // The xattrs objects need to be exported before the regular object they
            // refer to. Otherwise the importing logic won't have the xattrs available
            // when importing file content.
            self.append_xattrs(checksum, &xattrs)?;

            if let Some(instream) = instream {
                ensure!(meta.file_type() == gio::FileType::Regular);

                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(meta.size() as u64);
                let mut instream = BufReader::with_capacity(BUF_CAPACITY, instream.into_read());
                self.out
                    .append_data(&mut h, &path, &mut instream)
                    .with_context(|| format!("Writing regfile {}", checksum))?;
            } else {
                ensure!(meta.file_type() == gio::FileType::SymbolicLink);

                let target = meta
                    .symlink_target()
                    .ok_or_else(|| anyhow!("Missing symlink target"))?;
                let context = || format!("Writing content symlink: {}", checksum);
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_size(0);
                // Handle //chkconfig, see above
                if symlink_is_denormal(&target) {
                    h.set_link_name_literal(meta.symlink_target().unwrap().as_str())
                        .with_context(context)?;
                    self.out
                        .append_data(&mut h, &path, &mut std::io::empty())
                        .with_context(context)?;
                } else {
                    self.out
                        .append_link(&mut h, &path, target.as_str())
                        .with_context(context)?;
                }
            }
        }

        Ok((path, target_header))
    }

    /// Write a directory using the provided metadata.
    fn append_dir(&mut self, dirpath: &Utf8Path, meta: &ostree::DirMetaParsed) -> Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_uid(meta.uid as u64);
        header.set_gid(meta.gid as u64);
        header.set_mode(self.filter_mode(meta.mode));
        self.out
            .append_data(&mut header, dirpath, std::io::empty())?;
        Ok(())
    }

    /// Write a dirtree object.
    fn append_dirtree<C: IsA<gio::Cancellable>>(
        &mut self,
        dirpath: &Utf8Path,
        checksum: String,
        is_root: bool,
        cancellable: Option<&C>,
    ) -> Result<()> {
        let v = &self
            .repo
            .load_variant(ostree::ObjectType::DirTree, &checksum)?;
        self.append(ostree::ObjectType::DirTree, &checksum, v)?;
        drop(checksum);
        let v = v.data_as_bytes();
        let v = v.try_as_aligned()?;
        let v = gv_dirtree!().cast(v);
        let (files, dirs) = v.to_tuple();

        if let Some(c) = cancellable {
            c.set_error_if_cancelled()?;
        }

        for file in files {
            let (name, csum) = file.to_tuple();
            let name = name.to_str();
            let checksum = &hex::encode(csum);
            let (objpath, mut h) = self.append_content(checksum)?;
            h.set_entry_type(tar::EntryType::Link);
            h.set_link_name(&objpath)?;
            let subpath = &dirpath.join(name);
            let subpath = map_path(subpath);
            self.out
                .append_data(&mut h, &*subpath, &mut std::io::empty())?;
        }

        for item in dirs {
            let (name, contents_csum, meta_csum) = item.to_tuple();
            let name = name.to_str();
            let metadata = {
                let meta_csum = &hex::encode(meta_csum);
                let meta_v = &self
                    .repo
                    .load_variant(ostree::ObjectType::DirMeta, meta_csum)?;
                self.append(ostree::ObjectType::DirMeta, meta_csum, meta_v)?;
                // Safety: We passed the correct variant type just above
                ostree::DirMetaParsed::from_variant(meta_v).unwrap()
            };
            // Special hack because tar stream for containers can't have duplicates.
            if is_root && name == SYSROOT {
                continue;
            }
            let dirtree_csum = hex::encode(contents_csum);
            let subpath = &dirpath.join(name);
            let subpath = map_path(subpath);
            self.append_dir(&*subpath, &metadata)?;
            self.append_dirtree(&*subpath, dirtree_csum, false, cancellable)?;
        }

        Ok(())
    }
}

/// Recursively walk an OSTree commit and generate data into a `[tar::Builder]`
/// which contains all of the metadata objects, as well as a hardlinked
/// stream that looks like a checkout.  Extended attributes are stored specially out
/// of band of tar so that they can be reliably retrieved.
fn impl_export<W: std::io::Write>(
    repo: &ostree::Repo,
    commit_checksum: &str,
    out: &mut tar::Builder<W>,
    options: ExportOptions,
) -> Result<()> {
    let writer = &mut OstreeTarWriter::new(repo, out, options);
    writer.write_commit(commit_checksum)?;
    Ok(())
}

/// Configuration for tar export.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Format version; must be 0 or 1.
    pub format_version: u32,
}

/// Export an ostree commit to an (uncompressed) tar archive stream.
#[context("Exporting commit")]
pub fn export_commit(
    repo: &ostree::Repo,
    rev: &str,
    out: impl std::io::Write,
    options: Option<ExportOptions>,
) -> Result<()> {
    let commit = repo.require_rev(rev)?;
    let mut tar = tar::Builder::new(out);
    let options = options.unwrap_or_default();
    impl_export(repo, commit.as_str(), &mut tar, options)?;
    tar.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_path() {
        assert_eq!(map_path("/".into()), Utf8Path::new("/"));
        assert_eq!(
            map_path("./usr/etc/blah".into()),
            Utf8Path::new("./etc/blah")
        );
    }

    #[test]
    fn test_denormal_symlink() {
        let normal = ["/", "/usr", "../usr/bin/blah"];
        let denormal = ["../../usr/sbin//chkconfig", "foo//bar/baz"];
        for path in normal {
            assert!(!symlink_is_denormal(path));
        }
        for path in denormal {
            assert!(symlink_is_denormal(path));
        }
    }

    #[test]
    fn test_v0_xattrs_path() {
        let checksum = "b8627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7";
        let expected = "sysroot/ostree/repo/xattrs/b8627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7";
        let output = v0_xattrs_path(checksum);
        assert_eq!(&output, expected);
    }

    #[test]
    fn test_v0_xattrs_object_path() {
        let checksum = "b8627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7";
        let expected = "sysroot/ostree/repo/objects/b8/627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7.file.xattrs";
        let output = v0_xattrs_object_path(checksum);
        assert_eq!(&output, expected);
    }

    #[test]
    fn test_v1_xattrs_object_path() {
        let checksum = "b8627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7";
        let expected = "sysroot/ostree/repo/objects/b8/627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7.file-xattrs";
        let output = v1_xattrs_object_path(checksum);
        assert_eq!(&output, expected);
    }

    #[test]
    fn test_v1_xattrs_link_object_path() {
        let checksum = "b8627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7";
        let expected = "sysroot/ostree/repo/objects/b8/627e3ef0f255a322d2bd9610cfaaacc8f122b7f8d17c0e7e3caafa160f9fc7.file-xattrs-link";
        let output = v1_xattrs_link_object_path(checksum);
        assert_eq!(&output, expected);
    }
}
