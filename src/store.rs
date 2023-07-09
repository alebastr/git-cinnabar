/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::OsStr;
use std::io::{copy, BufRead, BufReader, Read, Write};
use std::iter::{repeat, IntoIterator};
use std::mem;
use std::num::NonZeroU32;
use std::os::raw::{c_char, c_int};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Mutex;

use bit_vec::BitVec;
use bstr::{BStr, BString, ByteSlice};
use cstr::cstr;
use derive_more::{Deref, Display};
use getset::{CopyGetters, Getters};
use indexmap::IndexMap;
use itertools::Itertools;
use once_cell::sync::Lazy;
use percent_encoding::{percent_decode, percent_encode, NON_ALPHANUMERIC};
use tee::TeeReader;
use url::{Host, Url};

use crate::graft::{graft, grafted, GraftError};
use crate::hg_bundle::{
    read_rev_chunk, rev_chunk, BundlePartInfo, BundleSpec, BundleWriter, RevChunkIter,
};
use crate::hg_connect_http::HttpRequest;
use crate::hg_data::{GitAuthorship, HgAuthorship, HgCommitter};
use crate::libcinnabar::{generate_manifest, git2hg, hg2git, hg_object_id};
use crate::libgit::{
    get_oid_blob, get_oid_committish, ls_tree, object_id, strbuf, BlobId, Commit, CommitId,
    RawBlob, RawCommit, RefTransaction, TreeId,
};
use crate::oid::{GitObjectId, HgObjectId, ObjectId};
use crate::progress::{progress_enabled, Progress};
use crate::util::{FromBytes, ImmutBString, OsStrExt, ReadExt, SliceExt, ToBoxed};
use crate::xdiff::{apply, textdiff, PatchInfo};
use crate::{check_enabled, do_reload, Checks};
use crate::{oid_type, set_metadata_to, MetadataFlags};

pub const REFS_PREFIX: &str = "refs/cinnabar/";
pub const REPLACE_REFS_PREFIX: &str = "refs/cinnabar/replace/";
pub const METADATA_REF: &str = "refs/cinnabar/metadata";
pub const CHECKED_REF: &str = "refs/cinnabar/checked";
pub const BROKEN_REF: &str = "refs/cinnabar/broken";
pub const NOTES_REF: &str = "refs/notes/cinnabar";

extern "C" {
    static metadata_flags: c_int;
}

pub fn has_metadata() -> bool {
    unsafe { metadata_flags != 0 }
}

macro_rules! hg2git {
    ($h:ident => $g:ident($i:ident)) => {
        oid_type!($g($i));
        oid_type!($h(HgObjectId));

        impl $h {
            pub fn to_git(&self) -> Option<$g> {
                unsafe {
                    hg2git
                        .get_note(&self)
                        .map(|o| $g::from_unchecked($i::from_unchecked(o)))
                }
            }
        }
    };
}

hg2git!(HgChangesetId => GitChangesetId(CommitId));
hg2git!(HgManifestId => GitManifestId(CommitId));
hg2git!(HgFileId => GitFileId(BlobId));

oid_type!(GitChangesetMetadataId(BlobId));
oid_type!(GitFileMetadataId(BlobId));

impl GitChangesetId {
    pub fn to_hg(&self) -> Option<HgChangesetId> {
        //TODO: avoid repeatedly reading metadata for a given changeset.
        //The equivalent python code was keeping a LRU cache.
        let metadata = RawGitChangesetMetadata::read(self);
        metadata
            .as_ref()
            .and_then(RawGitChangesetMetadata::parse)
            .map(|m| m.changeset_id().clone())
    }
}

pub struct RawGitChangesetMetadata(RawBlob);

impl RawGitChangesetMetadata {
    pub fn read(changeset_id: &GitChangesetId) -> Option<Self> {
        let note = unsafe { git2hg.get_note(changeset_id).map(BlobId::from_unchecked)? };
        RawBlob::read(&note).map(Self)
    }

    pub fn parse(&self) -> Option<ParsedGitChangesetMetadata> {
        let mut changeset = None;
        let mut manifest = None;
        let mut author = None;
        let mut extra = None;
        let mut files = None;
        let mut patch = None;
        for line in ByteSlice::lines(self.0.as_bytes()) {
            match line.splitn_exact(b' ')? {
                [b"changeset", c] => changeset = Some(HgChangesetId::from_bytes(c).ok()?),
                [b"manifest", m] => manifest = Some(HgManifestId::from_bytes(m).ok()?),
                [b"author", a] => author = Some(a),
                [b"extra", e] => extra = Some(e),
                [b"files", f] => files = Some(f),
                [b"patch", p] => patch = Some(p),
                _ => None?,
            }
        }

        Some(ParsedGitChangesetMetadata {
            changeset_id: changeset?,
            manifest_id: manifest.unwrap_or_else(HgManifestId::null),
            author,
            extra,
            files,
            patch,
        })
    }
}

#[derive(CopyGetters, Eq, Getters)]
pub struct GitChangesetMetadata<B: AsRef<[u8]>> {
    #[getset(get = "pub")]
    changeset_id: HgChangesetId,
    #[getset(get = "pub")]
    manifest_id: HgManifestId,
    author: Option<B>,
    extra: Option<B>,
    files: Option<B>,
    patch: Option<B>,
}

impl<B: AsRef<[u8]>, B2: AsRef<[u8]>> PartialEq<GitChangesetMetadata<B>>
    for GitChangesetMetadata<B2>
{
    fn eq(&self, other: &GitChangesetMetadata<B>) -> bool {
        self.changeset_id == other.changeset_id
            && self.manifest_id == other.manifest_id
            && self.author.as_ref().map(B2::as_ref) == other.author.as_ref().map(B::as_ref)
            && self.extra.as_ref().map(B2::as_ref) == other.extra.as_ref().map(B::as_ref)
            && self.files.as_ref().map(B2::as_ref) == other.files.as_ref().map(B::as_ref)
            && self.patch.as_ref().map(B2::as_ref) == other.patch.as_ref().map(B::as_ref)
    }
}

pub type ParsedGitChangesetMetadata<'a> = GitChangesetMetadata<&'a [u8]>;

impl<B: AsRef<[u8]>> GitChangesetMetadata<B> {
    pub fn author(&self) -> Option<&[u8]> {
        self.author.as_ref().map(B::as_ref)
    }

    pub fn extra(&self) -> Option<ChangesetExtra> {
        self.extra
            .as_ref()
            .map(|b| ChangesetExtra::from(b.as_ref()))
    }

    pub fn files(&self) -> impl Iterator<Item = &[u8]> {
        let mut split = self
            .files
            .as_ref()
            .map_or(&b""[..], B::as_ref)
            .split(|&b| b == b'\0');
        if self.files.is_none() {
            // b"".split() would return an empty first item, and we want to skip that.
            split.next();
        }
        split
    }

    pub fn patch(&self) -> Option<GitChangesetPatch> {
        self.patch.as_ref().map(|b| GitChangesetPatch(b.as_ref()))
    }

    pub fn serialize(&self) -> ImmutBString {
        // TODO: ideally, this would return a RawGitChangesetMetadata.
        let mut buf = Vec::new();
        writeln!(buf, "changeset {}", self.changeset_id()).unwrap();
        if self.manifest_id() != &HgManifestId::null() {
            writeln!(buf, "manifest {}", self.manifest_id()).unwrap();
        }
        for (key, value) in [
            (&b"author "[..], self.author.as_ref()),
            (&b"extra "[..], self.extra.as_ref()),
            (&b"files "[..], self.files.as_ref()),
            (&b"patch "[..], self.patch.as_ref()),
        ] {
            if let Some(value) = value {
                buf.extend_from_slice(key);
                buf.extend_from_slice(value.as_ref());
                buf.extend_from_slice(b"\n");
            }
        }
        // Remove final '\n'
        buf.pop();
        buf.into()
    }
}

pub type GeneratedGitChangesetMetadata = GitChangesetMetadata<ImmutBString>;

impl GeneratedGitChangesetMetadata {
    pub fn generate(
        commit: &Commit,
        changeset_id: &HgChangesetId,
        raw_changeset: &RawHgChangeset,
    ) -> Option<Self> {
        let changeset = raw_changeset.parse()?;
        let changeset_id = changeset_id.clone();
        let manifest_id = changeset.manifest().clone();
        let author = HgAuthorship::from(GitAuthorship(commit.author())).author;
        let author = if &*author != changeset.author() {
            Some(changeset.author().to_vec().into_boxed_slice())
        } else {
            None
        };
        let extra = changeset.extra().and_then(|mut e| {
            let mut buf = Vec::new();
            if e.get(b"committer") == Some(&HgCommitter::from(GitAuthorship(commit.committer())).0)
            {
                e.unset(b"committer");
                if e.is_empty() {
                    return None;
                }
            }
            e.dump_into(&mut buf);
            Some(buf.into_boxed_slice())
        });
        let files = changeset
            .files()
            .map(|files| bstr::join(b"\0", files).into_boxed_slice());
        let mut temp = GeneratedGitChangesetMetadata {
            changeset_id,
            manifest_id,
            author,
            extra,
            files,
            patch: None,
        };
        let new = RawHgChangeset::from_metadata(commit, &temp)?;
        if **raw_changeset != *new {
            // TODO: produce a better patch (byte_diff). In the meanwhile, we
            // do an approximation by taking the by-line diff from textdiff
            // and eliminating common parts, which is good enough.
            temp.patch = Some(GitChangesetPatch::from_patch_info(
                textdiff(&new, raw_changeset).map(|p| {
                    let orig = &new[p.start..p.end];
                    let patched = p.data;
                    let common_prefix = Iterator::zip(orig.iter(), patched.iter())
                        .take_while(|(&a, &b)| a == b)
                        .count();
                    let common_suffix = Iterator::zip(orig.iter().rev(), patched.iter().rev())
                        .take_while(|(&a, &b)| a == b)
                        .count();
                    PatchInfo {
                        start: p.start + common_prefix,
                        end: p.end - common_suffix,
                        data: &p.data[common_prefix..p.data.len() - common_suffix],
                    }
                }),
            ));
        }
        Some(temp)
    }
}

pub struct ChangesetExtra<'a> {
    data: BTreeMap<&'a BStr, &'a BStr>,
}

impl<'a> ChangesetExtra<'a> {
    fn from(buf: &'a [u8]) -> Self {
        if buf.is_empty() {
            ChangesetExtra::new()
        } else {
            ChangesetExtra {
                data: buf
                    .split(|&c| c == b'\0')
                    .map(|a| {
                        let [k, v] = a.splitn_exact(b':').unwrap();
                        (k.as_bstr(), v.as_bstr())
                    })
                    .collect(),
            }
        }
    }

    pub fn new() -> Self {
        ChangesetExtra {
            data: BTreeMap::new(),
        }
    }

    pub fn get(&self, name: &[u8]) -> Option<&'a [u8]> {
        self.data.get(name.as_bstr()).map(|b| &***b)
    }

    pub fn unset(&mut self, name: &[u8]) {
        self.data.remove(name.as_bstr());
    }

    pub fn set(&mut self, name: &'a [u8], value: &'a [u8]) {
        self.data.insert(name.as_bstr(), value.as_bstr());
    }

    pub fn dump_into(&self, buf: &mut Vec<u8>) {
        for b in Itertools::intersperse(
            self.data.iter().map(|(&k, &v)| {
                let mut buf = Vec::new();
                buf.extend_from_slice(k);
                buf.push(b':');
                buf.extend_from_slice(v);
                Cow::Owned(buf)
            }),
            Cow::Borrowed(&b"\0"[..]),
        ) {
            buf.extend_from_slice(&b);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[test]
fn test_changeset_extra() {
    let mut extra = ChangesetExtra::new();
    extra.set(b"foo", b"bar");
    extra.set(b"bar", b"qux");
    let mut result = Vec::new();
    extra.dump_into(&mut result);
    assert_eq!(result.as_bstr(), b"bar:qux\0foo:bar".as_bstr());

    let mut extra = ChangesetExtra::from(&result);
    let mut result2 = Vec::new();
    extra.dump_into(&mut result2);
    assert_eq!(result.as_bstr(), result2.as_bstr());

    extra.set(b"aaaa", b"bbbb");
    result2.truncate(0);
    extra.dump_into(&mut result2);
    assert_eq!(result2.as_bstr(), b"aaaa:bbbb\0bar:qux\0foo:bar".as_bstr());
}

pub struct GitChangesetPatch<'a>(&'a [u8]);

impl<'a> GitChangesetPatch<'a> {
    pub fn iter(&self) -> Option<impl Iterator<Item = PatchInfo<Cow<'a, [u8]>>>> {
        self.0
            .split(|c| *c == b'\0')
            .map(|part| {
                let [start, end, data] = part.splitn_exact(b',')?;
                let start = usize::from_bytes(start).ok()?;
                let end = usize::from_bytes(end).ok()?;
                let data = Cow::from(percent_decode(data));
                Some(PatchInfo { start, end, data })
            })
            .collect::<Option<Vec<_>>>()
            .map(IntoIterator::into_iter)
    }

    pub fn apply(&self, input: &[u8]) -> Option<ImmutBString> {
        Some(apply(self.iter()?, input))
    }

    pub fn from_patch_info(
        iter: impl Iterator<Item = PatchInfo<impl AsRef<[u8]>>>,
    ) -> ImmutBString {
        let mut result = Vec::new();
        for (n, part) in iter.enumerate() {
            if n > 0 {
                result.push(b'\0');
            }
            write!(
                result,
                "{},{},{}",
                part.start,
                part.end,
                percent_encode(part.data.as_ref(), NON_ALPHANUMERIC)
            )
            .ok();
        }
        result.into_boxed_slice()
    }
}

#[derive(Deref)]
#[deref(forward)]
pub struct RawHgChangeset(pub ImmutBString);

impl RawHgChangeset {
    pub fn from_metadata<B: AsRef<[u8]>>(
        commit: &Commit,
        metadata: &GitChangesetMetadata<B>,
    ) -> Option<Self> {
        let HgAuthorship {
            author: mut hg_author,
            timestamp: hg_timestamp,
            utcoffset: hg_utcoffset,
        } = GitAuthorship(commit.author()).into();

        if let Some(author) = metadata.author() {
            hg_author = author.to_boxed();
        }
        let mut extra = metadata.extra();
        let hg_committer = (extra.as_ref().and_then(|e| e.get(b"committer")).is_none()
            && (commit.author() != commit.committer()))
        .then(|| HgCommitter::from(GitAuthorship(commit.committer())).0);

        if let Some(hg_committer) = hg_committer.as_ref() {
            extra
                .get_or_insert_with(ChangesetExtra::new)
                .set(b"committer", hg_committer);
        }

        let mut changeset = Vec::new();
        writeln!(changeset, "{}", metadata.manifest_id()).ok()?;
        changeset.extend_from_slice(&hg_author);
        changeset.push(b'\n');
        changeset.extend_from_slice(&hg_timestamp);
        changeset.push(b' ');
        changeset.extend_from_slice(&hg_utcoffset);
        if let Some(extra) = extra {
            changeset.push(b' ');
            extra.dump_into(&mut changeset);
        }
        let mut files = metadata.files().collect_vec();
        //TODO: probably don't actually need sorting.
        files.sort();
        for f in &files {
            changeset.push(b'\n');
            changeset.extend_from_slice(f);
        }
        changeset.extend_from_slice(b"\n\n");
        changeset.extend_from_slice(commit.body());

        if let Some(patch) = metadata.patch() {
            let mut patched = patch.apply(&changeset)?.to_vec();
            mem::swap(&mut changeset, &mut patched);
        }

        // Adjust for `handle_changeset_conflict`.
        // TODO: when creating the git2hg metadata moves to Rust, we can
        // create a patch instead, which would be handled above instead of
        // manually here.
        let node = metadata.changeset_id();
        if *node != HgChangesetId::null() {
            while changeset[changeset.len() - 1] == b'\0' {
                let mut hash = HgChangesetId::create();
                let mut parents = commit
                    .parents()
                    .iter()
                    .map(|p| GitChangesetId::from_unchecked(p.clone()).to_hg())
                    .chain(repeat(Some(HgChangesetId::null())))
                    .take(2)
                    .collect::<Option<Vec<_>>>()?;
                parents.sort();
                for p in parents {
                    hash.update(p.as_raw_bytes());
                }
                hash.update(&changeset);
                if hash.finalize() == *node {
                    break;
                }
                changeset.pop();
            }
        }
        Some(RawHgChangeset(changeset.into()))
    }

    pub fn read(oid: &GitChangesetId) -> Option<Self> {
        let commit = RawCommit::read(oid)?;
        let commit = commit.parse()?;
        let metadata = RawGitChangesetMetadata::read(oid)?;
        let metadata = metadata.parse()?;
        Self::from_metadata(&commit, &metadata)
    }

    pub fn parse(&self) -> Option<HgChangeset> {
        let [header, body] = self.0.splitn_exact(&b"\n\n"[..])?;
        let mut lines = header.splitn(4, |&b| b == b'\n');
        let manifest = lines.next()?;
        let author = lines.next()?;
        let mut date = lines.next()?.splitn(3, |&b| b == b' ');
        let timestamp = date.next()?;
        let utcoffset = date.next()?;
        let extra = date.next();
        let files = lines.next();
        Some(HgChangeset {
            manifest: HgManifestId::from_bytes(manifest).ok()?,
            author,
            timestamp,
            utcoffset,
            extra,
            files,
            body,
        })
    }
}

#[derive(CopyGetters, Getters)]
pub struct HgChangeset<'a> {
    #[getset(get = "pub")]
    manifest: HgManifestId,
    #[getset(get_copy = "pub")]
    author: &'a [u8],
    #[getset(get_copy = "pub")]
    timestamp: &'a [u8],
    #[getset(get_copy = "pub")]
    utcoffset: &'a [u8],
    extra: Option<&'a [u8]>,
    files: Option<&'a [u8]>,
    #[getset(get_copy = "pub")]
    body: &'a [u8],
}

impl<'a> HgChangeset<'a> {
    pub fn extra(&self) -> Option<ChangesetExtra> {
        self.extra.map(ChangesetExtra::from)
    }

    pub fn files(&self) -> Option<impl Iterator<Item = &[u8]>> {
        self.files.as_ref().map(|b| b.split(|&b| b == b'\n'))
    }
}

#[derive(Deref)]
#[deref(forward)]
pub struct RawHgManifest(pub ImmutBString);

impl RawHgManifest {
    pub fn read(oid: &GitManifestId) -> Option<Self> {
        unsafe {
            generate_manifest(&(***oid).clone().into())
                .as_ref()
                .map(|b| Self(b.as_bytes().to_owned().into()))
        }
    }
}

#[derive(Deref)]
#[deref(forward)]
pub struct RawHgFile(pub ImmutBString);

impl RawHgFile {
    pub fn read(oid: &GitFileId, metadata: Option<&GitFileMetadataId>) -> Option<Self> {
        let mut result = Vec::new();
        if let Some(metadata) = metadata {
            result.extend_from_slice(b"\x01\n");
            result.extend_from_slice(RawBlob::read(metadata)?.as_bytes());
            result.extend_from_slice(b"\x01\n");
        }
        result.extend_from_slice(RawBlob::read(oid)?.as_bytes());
        Some(Self(result.into()))
    }
}

#[derive(Debug, Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct DagNodeId(NonZeroU32);

impl DagNodeId {
    fn try_from_offset(offset: usize) -> Option<Self> {
        u32::try_from(offset + 1)
            .ok()
            .and_then(NonZeroU32::new)
            .map(Self)
    }

    fn to_offset(self) -> usize {
        self.0.get() as usize - 1
    }
}

#[derive(Debug)]
struct DagNode<N, T> {
    node: N,
    parent1: Option<DagNodeId>,
    parent2: Option<DagNodeId>,
    data: T,
}

#[derive(Debug)]
struct ChangesetInfo {
    has_children: bool,
    branch: BString,
}

#[derive(Debug)]
pub struct Dag<N, T> {
    // 4 billion nodes ought to be enough for anybody.
    ids: BTreeMap<N, DagNodeId>,
    dag: Vec<DagNode<N, T>>,
}

pub enum Traversal {
    Parents,
    Children,
}

impl<N: Ord + Clone, T> Dag<N, T> {
    pub fn new() -> Self {
        Dag {
            ids: BTreeMap::new(),
            dag: Vec::new(),
        }
    }

    pub fn add<F: FnMut(DagNodeId, &mut T)>(
        &mut self,
        node: &N,
        parents: &[&N],
        data: T,
        mut cb: F,
    ) -> DagNodeId {
        assert!(parents.len() <= 2);
        let parents = parents
            .iter()
            .filter_map(|p| {
                self.get_mut(p).map(|(id, data)| {
                    cb(id, data);
                    id
                })
            })
            .collect_vec();
        let id = DagNodeId::try_from_offset(self.dag.len()).unwrap();
        assert!(self.ids.insert(node.clone(), id).is_none());
        self.dag.push(DagNode {
            node: node.clone(),
            parent1: parents.get(0).copied(),
            parent2: parents.get(1).copied(),
            data,
        });
        id
    }

    pub fn get(&self, node: &N) -> Option<(DagNodeId, &T)> {
        self.ids
            .get(node)
            .map(|id| (*id, &self.dag[id.to_offset()].data))
    }

    pub fn get_mut(&mut self, node: &N) -> Option<(DagNodeId, &mut T)> {
        self.ids
            .get(node)
            .map(|id| (*id, &mut self.dag[id.to_offset()].data))
    }

    pub fn get_by_id(&self, id: DagNodeId) -> (&N, &T) {
        let node = &self.dag[id.to_offset()];
        (&node.node, &node.data)
    }

    pub fn traverse_mut(
        &mut self,
        start: &N,
        direction: Traversal,
        cb: impl FnMut(&N, &mut T) -> bool,
    ) {
        match (direction, self.ids.get(start)) {
            (Traversal::Parents, Some(&start)) => self.traverse_parents_mut(start, cb),
            (Traversal::Children, Some(&start)) => self.traverse_children_mut(start, cb),
            _ => {}
        }
    }

    fn traverse_parents_mut(&mut self, start: DagNodeId, mut cb: impl FnMut(&N, &mut T) -> bool) {
        let mut queue = VecDeque::from([start]);
        let mut seen = BitVec::from_elem(self.ids.len(), false);
        while let Some(id) = queue.pop_front() {
            seen.set(id.to_offset(), true);
            let node = &mut self.dag[id.to_offset()];
            if cb(&node.node, &mut node.data) {
                for id in [node.parent1, node.parent2].into_iter().flatten() {
                    if !seen[id.to_offset()] {
                        queue.push_back(id);
                    }
                }
            }
        }
    }

    fn traverse_children_mut(&mut self, start: DagNodeId, mut cb: impl FnMut(&N, &mut T) -> bool) {
        let mut seen = BitVec::from_elem(self.ids.len() - start.to_offset(), false);
        for (idx, node) in self.dag[start.to_offset()..].iter_mut().enumerate() {
            if (idx == 0
                || [node.parent1, node.parent2]
                    .into_iter()
                    .flatten()
                    .any(|id| id >= start && seen[id.to_offset() - start.to_offset()]))
                && cb(&node.node, &mut node.data)
            {
                seen.set(idx, true);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&N, &T)> {
        self.dag.iter().map(|node| (&node.node, &node.data))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&N, &mut T)> {
        self.dag.iter_mut().map(|node| (&node.node, &mut node.data))
    }
}

#[derive(Debug)]
pub struct ChangesetHeads {
    dag: Dag<HgChangesetId, ChangesetInfo>,
    heads: BTreeSet<DagNodeId>,
}

impl ChangesetHeads {
    pub fn new() -> Self {
        ChangesetHeads {
            dag: Dag::new(),
            heads: BTreeSet::new(),
        }
    }

    fn from_stored_metadata() -> Self {
        get_oid_committish(b"refs/cinnabar/metadata^1")
            .as_ref()
            .map_or_else(ChangesetHeads::new, ChangesetHeads::from_metadata)
    }

    pub fn from_metadata(cid: &CommitId) -> Self {
        let mut result = ChangesetHeads::new();

        let commit = RawCommit::read(cid).unwrap();
        let commit = commit.parse().unwrap();
        for l in ByteSlice::lines(commit.body()) {
            let [h, b] = l.splitn_exact(b' ').unwrap();
            let cs = HgChangesetId::from_bytes(h).unwrap();
            result.add(&cs, &[], b.as_bstr());
        }
        result
    }

    pub fn add(&mut self, cs: &HgChangesetId, parents: &[&HgChangesetId], branch: &BStr) {
        let data = ChangesetInfo {
            has_children: false,
            branch: BString::from(branch),
        };
        let id = self.dag.add(cs, parents, data, |parent_id, parent_data| {
            parent_data.has_children = true;
            if parent_data.branch == branch {
                self.heads.remove(&parent_id);
            }
        });
        self.heads.insert(id);
    }

    pub fn branch_heads(&self) -> impl Iterator<Item = (&HgChangesetId, &BStr)> {
        self.heads.iter().map(|id| {
            let (node, data) = self.dag.get_by_id(*id);
            (node, data.branch.as_bstr())
        })
    }

    pub fn heads(&self) -> impl Iterator<Item = &HgChangesetId> {
        self.heads.iter().filter_map(|id| {
            let (node, data) = self.dag.get_by_id(*id);
            // Branch heads can have children in other branches, in which case
            // they are not heads.
            (!data.has_children).then(|| node)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }
}

pub static CHANGESET_HEADS: Lazy<Mutex<ChangesetHeads>> =
    Lazy::new(|| Mutex::new(ChangesetHeads::from_stored_metadata()));

#[derive(Default)]
pub struct TagSet {
    tags: IndexMap<Box<[u8]>, (HgChangesetId, HashSet<HgChangesetId>)>,
}

impl TagSet {
    pub fn from_buf(buf: &[u8]) -> Option<Self> {
        let mut tags = IndexMap::new();
        for line in ByteSlice::lines(buf) {
            if line.is_empty() {
                continue;
            }
            let [node, tag] = line.splitn_exact(b' ')?;
            let tag = tag.trim_with(|b| b.is_ascii_whitespace());
            let node = HgChangesetId::from_bytes(node).ok()?;
            tags.entry(tag.to_boxed())
                .and_modify(|e: &mut (HgChangesetId, HashSet<HgChangesetId>)| {
                    let mut node = node.clone();
                    mem::swap(&mut e.0, &mut node);
                    e.1.insert(node);
                })
                .or_insert_with(|| (node, HashSet::new()));
        }
        Some(TagSet { tags })
    }

    pub fn merge(&mut self, other: TagSet) {
        if self.tags.is_empty() {
            self.tags = other.tags;
            return;
        }
        for (tag, (anode, ahist)) in other.tags.into_iter() {
            // Derived from mercurial's _updatetags.
            self.tags
                .entry(tag)
                .and_modify(|(bnode, bhist)| {
                    if !(bnode != &anode
                        && bhist.contains(&anode)
                        && (!ahist.contains(bnode) || bhist.len() > ahist.len()))
                    {
                        *bnode = anode.clone();
                    }
                    bhist.extend(ahist.iter().cloned());
                })
                .or_insert((anode, ahist));
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &HgChangesetId)> {
        self.tags
            .iter()
            .filter_map(|(tag, (node, _))| (node != &HgChangesetId::null()).then(|| (&**tag, node)))
    }
}

impl PartialEq for TagSet {
    fn eq(&self, other: &Self) -> bool {
        self.iter().sorted().collect_vec() == other.iter().sorted().collect_vec()
    }
}

pub fn get_tags() -> TagSet {
    let mut tags = TagSet::default();
    let mut tags_files = HashSet::new();
    for head in CHANGESET_HEADS.lock().unwrap().heads() {
        (|| -> Option<()> {
            let head = head.to_git()?;
            let tags_file = get_oid_blob(format!("{}:.hgtags", head).as_bytes())?;
            if tags_files.insert(tags_file.clone()) {
                let tags_blob = RawBlob::read(&tags_file).unwrap();
                tags.merge(TagSet::from_buf(tags_blob.as_bytes())?);
            }
            Some(())
        })();
    }
    tags
}

static BUNDLE_BLOB: Lazy<Mutex<Option<object_id>>> = Lazy::new(|| Mutex::new(None));

#[no_mangle]
pub unsafe extern "C" fn store_changesets_metadata(result: *mut object_id) {
    let result = result.as_mut().unwrap();
    let mut tree = strbuf::new();
    if let Some(blob) = &*BUNDLE_BLOB.lock().unwrap() {
        let blob = BlobId::from_unchecked(GitObjectId::from(blob.clone()));
        tree.extend_from_slice(b"100644 bundle\0");
        tree.extend_from_slice(blob.as_raw_bytes());
    }
    let mut tid = object_id::default();
    store_git_tree(&tree, std::ptr::null(), &mut tid);
    drop(tree);
    let mut commit = strbuf::new();
    writeln!(commit, "tree {}", GitObjectId::from(tid)).ok();
    let heads = CHANGESET_HEADS.lock().unwrap();
    for (head, _) in heads.branch_heads() {
        writeln!(commit, "parent {}", head.to_git().unwrap()).ok();
    }
    writeln!(commit, "author  <cinnabar@git> 0 +0000").ok();
    writeln!(commit, "committer  <cinnabar@git> 0 +0000").ok();
    for (head, branch) in heads.branch_heads() {
        write!(commit, "\n{} {}", head, branch).ok();
    }
    store_git_commit(&commit, result);
}

#[no_mangle]
pub unsafe extern "C" fn reset_changeset_heads() {
    let mut heads = CHANGESET_HEADS.lock().unwrap();
    *heads = ChangesetHeads::from_stored_metadata();
}

pub fn set_changeset_heads(new_heads: ChangesetHeads) {
    let mut heads = CHANGESET_HEADS.lock().unwrap();
    *heads = new_heads;
}

extern "C" {
    pub fn ensure_store_init();
    pub fn store_git_blob(blob_buf: *const strbuf, result: *mut object_id);
    fn store_git_tree(tree_buf: *const strbuf, reference: *const object_id, result: *mut object_id);
    fn store_git_commit(commit_buf: *const strbuf, result: *mut object_id);
    pub fn do_set(what: *const c_char, hg_id: *const hg_object_id, git_id: *const object_id);
    pub fn do_set_replace(replaced: *const object_id, replace_with: *const object_id);
    fn create_git_tree(
        tree_id: *const object_id,
        ref_tree: *const object_id,
        result: *mut object_id,
    );
    pub fn reset_manifest_heads();
    fn ensure_empty_tree() -> *const object_id;
}

fn store_changeset(
    changeset_id: &HgChangesetId,
    parents: &[HgChangesetId],
    raw_changeset: &RawHgChangeset,
) -> Result<(CommitId, Option<CommitId>), GraftError> {
    let git_parents = parents
        .iter()
        .map(HgChangesetId::to_git)
        .collect::<Option<Vec<_>>>()
        .ok_or(GraftError::NoGraft)?;
    let changeset = raw_changeset.parse().unwrap();
    let manifest_tree_id = match changeset.manifest() {
        m if m == &HgManifestId::null() => unsafe {
            TreeId::from_unchecked(GitObjectId::from(
                ensure_empty_tree().as_ref().unwrap().clone(),
            ))
        },
        m => {
            let git_manifest_id = m.to_git().unwrap();
            let manifest_commit = RawCommit::read(&git_manifest_id).unwrap();
            let manifest_commit = manifest_commit.parse().unwrap();
            manifest_commit.tree().clone()
        }
    };

    let ref_tree = git_parents.get(0).map(|p| {
        let ref_commit = RawCommit::read(p).unwrap();
        let ref_commit = ref_commit.parse().unwrap();
        object_id::from(ref_commit.tree())
    });

    let mut tree_id = object_id::default();
    unsafe {
        create_git_tree(
            &object_id::from(manifest_tree_id),
            ref_tree
                .as_ref()
                .map_or(std::ptr::null(), |t| t as *const _),
            &mut tree_id,
        );
    }
    let tree_id = TreeId::from_unchecked(GitObjectId::from(tree_id));

    let (commit_id, metadata_id, transition) =
        match graft(changeset_id, raw_changeset, &tree_id, &git_parents) {
            Ok(Some(commit_id)) => {
                let metadata = GeneratedGitChangesetMetadata::generate(
                    &RawCommit::read(&commit_id).unwrap().parse().unwrap(),
                    changeset_id,
                    raw_changeset,
                )
                .unwrap();
                if !grafted() && metadata.patch().is_some() {
                    (Some(commit_id), None, true)
                } else {
                    let mut buf = strbuf::new();
                    buf.extend_from_slice(&metadata.serialize());
                    let mut metadata_oid = object_id::default();
                    unsafe {
                        store_git_blob(&buf, &mut metadata_oid);
                    }
                    let metadata_id = GitChangesetMetadataId::from_unchecked(
                        BlobId::from_unchecked(GitObjectId::from(metadata_oid)),
                    );
                    (Some(commit_id), Some(metadata_id), false)
                }
            }
            Ok(None) | Err(GraftError::NoGraft) => (None, None, false),
            Err(e) => return Err(e),
        };

    let (commit_id, metadata_id, replace) = if commit_id.is_none() || transition {
        let replace = commit_id;
        let result = raw_commit_for_changeset(&changeset, &tree_id, &git_parents);
        let mut result_oid = object_id::default();
        unsafe {
            store_git_commit(&result, &mut result_oid);
        }
        let commit_id = CommitId::from_unchecked(GitObjectId::from(result_oid));

        let metadata = GeneratedGitChangesetMetadata::generate(
            &RawCommit::read(&commit_id).unwrap().parse().unwrap(),
            changeset_id,
            raw_changeset,
        )
        .unwrap();
        let mut buf = strbuf::new();
        buf.extend_from_slice(&metadata.serialize());
        let mut metadata_oid = object_id::default();
        unsafe {
            store_git_blob(&buf, &mut metadata_oid);
        }
        let metadata_id = GitChangesetMetadataId::from_unchecked(BlobId::from_unchecked(
            GitObjectId::from(metadata_oid),
        ));

        (commit_id, metadata_id, replace)
    } else {
        (
            unsafe { commit_id.unwrap_unchecked() },
            metadata_id.unwrap(),
            None,
        )
    };

    let result = (commit_id.clone(), replace);
    unsafe {
        let changeset_id = hg_object_id::from(changeset_id.clone());
        let commit_id = object_id::from(commit_id);
        let blob_id = object_id::from((*metadata_id).clone());
        if let Some(replace) = &result.1 {
            let replace = object_id::from(replace);
            do_set_replace(&replace, &commit_id);
        }
        do_set(cstr!("changeset").as_ptr(), &changeset_id, &commit_id);
        do_set(
            cstr!("changeset-metadata").as_ptr(),
            &changeset_id,
            &blob_id,
        );
    }

    let mut heads = CHANGESET_HEADS.lock().unwrap();
    let branch = changeset
        .extra()
        .and_then(|e| e.get(b"branch"))
        .unwrap_or(b"default")
        .as_bstr();
    let parents = parents.iter().collect::<Vec<_>>();
    heads.add(changeset_id, &parents, branch);
    Ok(result)
}

pub fn raw_commit_for_changeset(
    changeset: &HgChangeset,
    tree_id: &TreeId,
    parents: &[GitChangesetId],
) -> strbuf {
    let mut result = strbuf::new();
    let author = HgAuthorship {
        author: changeset.author(),
        timestamp: changeset.timestamp(),
        utcoffset: changeset.utcoffset(),
    };
    let git_author = GitAuthorship::from(author.clone());
    let git_committer = changeset
        .extra()
        .and_then(|extra| extra.get(b"committer"))
        .map(|committer| {
            if committer.ends_with(b">") {
                GitAuthorship::from(HgAuthorship {
                    author: committer,
                    timestamp: author.timestamp,
                    utcoffset: author.utcoffset,
                })
            } else {
                GitAuthorship::from(HgCommitter(committer))
            }
        });
    let git_committer = git_committer.as_ref().unwrap_or(&git_author);
    result.extend_from_slice(format!("tree {}\n", tree_id).as_bytes());
    for parent in parents {
        result.extend_from_slice(format!("parent {}\n", parent).as_bytes());
    }
    result.extend_from_slice(b"author ");
    result.extend_from_slice(&git_author.0);
    result.extend_from_slice(b"\ncommitter ");
    result.extend_from_slice(&git_committer.0);
    result.extend_from_slice(b"\n\n");
    result.extend_from_slice(changeset.body());
    result
}

pub fn create_changeset(
    commit_id: &CommitId,
    manifest_id: &HgManifestId,
    files: Option<Box<[u8]>>,
) -> (HgChangesetId, GitChangesetMetadataId) {
    let mut metadata = GitChangesetMetadata {
        changeset_id: HgChangesetId::null(),
        manifest_id: manifest_id.clone(),
        author: None,
        extra: None,
        files: files.and_then(|f| (!f.is_empty()).then(|| f)),
        patch: None,
    };
    let commit = RawCommit::read(commit_id).unwrap();
    let commit = commit.parse().unwrap();
    let branch = commit.parents().get(0).and_then(|p| {
        let metadata =
            RawGitChangesetMetadata::read(&GitChangesetId::from_unchecked(p.clone())).unwrap();
        let metadata = metadata.parse().unwrap();
        metadata
            .extra()
            .and_then(|e| e.get(b"branch").map(ToBoxed::to_boxed))
    });
    if let Some(branch) = &branch {
        let mut extra = ChangesetExtra::new();
        extra.set(b"branch", branch);
        let mut buf = Vec::new();
        extra.dump_into(&mut buf);
        metadata.extra = Some(buf.into_boxed_slice());
    }
    let changeset = RawHgChangeset::from_metadata(&commit, &metadata).unwrap();
    let mut hash = HgChangesetId::create();
    let parents = commit
        .parents()
        .iter()
        .map(|p| GitChangesetId::from_unchecked(p.clone()).to_hg())
        .collect::<Option<Vec<_>>>()
        .unwrap();
    for p in parents
        .iter()
        .cloned()
        .chain(repeat(HgChangesetId::null()))
        .take(2)
        .sorted()
        .collect_vec()
    {
        hash.update(p.as_raw_bytes());
    }
    hash.update(&changeset.0);
    metadata.changeset_id = hash.finalize();
    let mut buf = strbuf::new();
    buf.extend_from_slice(&metadata.serialize());
    let mut blob_oid = object_id::default();
    unsafe {
        let changeset_id = hg_object_id::from(metadata.changeset_id.clone());
        store_git_blob(&buf, &mut blob_oid);
        do_set(
            cstr!("changeset").as_ptr(),
            &changeset_id,
            &object_id::from(commit_id),
        );
        do_set(
            cstr!("changeset-metadata").as_ptr(),
            &changeset_id,
            &blob_oid,
        );
    }
    let mut heads = CHANGESET_HEADS.lock().unwrap();
    let branch = branch.as_deref().unwrap_or(b"default").as_bstr();
    let parents = parents.iter().collect::<Vec<_>>();
    heads.add(&metadata.changeset_id, &parents, branch);
    let metadata_id =
        GitChangesetMetadataId::from_unchecked(BlobId::from_unchecked(GitObjectId::from(blob_oid)));
    (metadata.changeset_id, metadata_id)
}

extern "C" {
    pub fn store_manifest(chunk: *const rev_chunk);
    fn store_file(chunk: *const rev_chunk);

    fn check_file(
        oid: *const hg_object_id,
        parent1: *const hg_object_id,
        parent2: *const hg_object_id,
    ) -> c_int;
}

static STORED_FILES: Lazy<Mutex<BTreeMap<HgChangesetId, [HgChangesetId; 2]>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

pub fn do_check_files() -> bool {
    // Try to detect issue #207 as early as possible.
    let mut busted = false;
    for (node, [p1, p2]) in STORED_FILES
        .lock()
        .unwrap()
        .iter()
        .progress(|n| format!("Checking {n} imported file root and head revisions"))
    {
        if unsafe { check_file(&node.clone().into(), &p1.clone().into(), &p2.clone().into()) } == 0
        {
            error!(target: "root", "Error in file {node}");
            busted = true;
        }
    }
    if busted {
        let mut transaction = RefTransaction::new().unwrap();
        transaction
            .update(
                BROKEN_REF,
                &unsafe {
                    CommitId::from_unchecked(GitObjectId::from(crate::libgit::metadata_oid.clone()))
                },
                None,
                "post-pull check",
            )
            .unwrap();
        transaction.commit().unwrap();
        error!(
            target: "root",
            "It seems you have hit a known, rare, and difficult to \
             reproduce issue.\n\
             Your help would be appreciated.\n\
             Please try either `git cinnabar rollback` followed by \
             the same command that just\n\
             failed, or `git cinnabar reclone`.\n\
             Please open a new issue \
             (https://github.com/glandium/git-cinnabar/issues/new)\n\
             mentioning issue #207 and reporting whether the second \
             attempt succeeded.\n\n\
             Please read all the above and keep a copy of this \
             repository."
        );
    }
    !busted
}

pub fn store_changegroup<R: Read>(input: R, version: u8) {
    unsafe {
        ensure_store_init();
    }
    let mut bundle = strbuf::new();
    let mut bundle_writer = None;
    let mut input = if check_enabled(Checks::UNBUNDLER) && env::var("GIT_DIR").is_ok() {
        bundle_writer = Some(BundleWriter::new(BundleSpec::V2Zstd, &mut bundle).unwrap());
        let bundle_writer = bundle_writer.as_mut().unwrap();
        let info =
            BundlePartInfo::new(0, "changegroup").set_param("version", &format!("{:02}", version));
        let part = bundle_writer.new_part(info).unwrap();
        Box::new(TeeReader::new(input, part)) as Box<dyn Read>
    } else {
        Box::from(input)
    };
    let mut changesets = Vec::new();
    for changeset in
        RevChunkIter::new(version, &mut input).progress(|n| format!("Reading {n} changesets"))
    {
        changesets.push(Box::new((
            HgChangesetId::from_unchecked(HgObjectId::from(changeset.delta_node().clone())),
            changeset,
        )));
    }
    for manifest in RevChunkIter::new(version, &mut input)
        .progress(|n| format!("Reading and importing {n} manifests"))
    {
        unsafe {
            store_manifest(&manifest);
        }
    }
    let files = Cell::new(0);
    let mut progress = repeat(()).progress(|n| {
        format!(
            "Reading and importing {n} revisions of {} files",
            files.get()
        )
    });
    let mut stored_files = STORED_FILES.lock().unwrap();
    let null_parents = [HgChangesetId::null(), HgChangesetId::null()];
    while {
        let mut buf = strbuf::new();
        read_rev_chunk(&mut input, &mut buf);
        !buf.as_bytes().is_empty()
    } {
        files.set(files.get() + 1);
        for (file, ()) in RevChunkIter::new(version, &mut input).zip(&mut progress) {
            let node = HgChangesetId::from_unchecked(HgObjectId::from(file.node().clone()));
            let parents = [
                HgChangesetId::from_unchecked(HgObjectId::from(file.parent1().clone())),
                HgChangesetId::from_unchecked(HgObjectId::from(file.parent2().clone())),
            ];
            // Try to detect issue #207 as early as possible.
            // Keep track of file roots of files with metadata and at least
            // one head that can be traced back to each of those roots.
            // Or, in the case of updates, all heads.
            if has_metadata()
                || stored_files.contains_key(&parents[0])
                || stored_files.contains_key(&parents[1])
            {
                stored_files.insert(node, parents.clone());
                for p in parents.into_iter() {
                    if p == HgChangesetId::null() {
                        continue;
                    }
                    if stored_files.get(&p) != Some(&null_parents) {
                        stored_files.remove(&p);
                    }
                }
            } else if parents == null_parents {
                if let Some(diff) = file.iter_diff().next() {
                    if diff.start == 0 && diff.data.get(..2) == Some(b"\x01\n") {
                        stored_files.insert(node, parents);
                    }
                }
            }
            unsafe {
                store_file(&file);
            }
        }
    }
    drop(progress);

    let mut previous = (HgChangesetId::null(), RawHgChangeset(Box::new([])));
    for changeset in changesets
        .drain(..)
        .progress(|n| format!("Importing {n} changesets"))
    {
        let (delta_node, changeset) = &*changeset;
        let changeset_id =
            HgChangesetId::from_unchecked(HgObjectId::from(changeset.node().clone()));
        let parents = [changeset.parent1(), changeset.parent2()]
            .into_iter()
            .filter_map(|p| {
                let p = HgChangesetId::from_unchecked(HgObjectId::from(p.clone()));
                (p != HgChangesetId::null()).then(|| p)
            })
            .collect::<Vec<_>>();

        let reference_cs = if delta_node == &previous.0 {
            previous.1
        } else if delta_node == &HgChangesetId::null() {
            RawHgChangeset(Box::new([]))
        } else {
            RawHgChangeset::read(&delta_node.to_git().unwrap()).unwrap()
        };

        let mut last_end = 0;
        let mut raw_changeset = Vec::new();
        for diff in changeset.iter_diff() {
            if diff.start > reference_cs.len() || diff.start < last_end {
                die!("Malformed changeset chunk for {changeset_id}");
            }
            raw_changeset.extend_from_slice(&reference_cs[last_end..diff.start]);
            raw_changeset.extend_from_slice(&diff.data);
            last_end = diff.end;
        }
        if reference_cs.len() < last_end {
            die!("Malformed changeset chunk for {changeset_id}");
        }
        raw_changeset.extend_from_slice(&reference_cs[last_end..]);
        let raw_changeset = RawHgChangeset(raw_changeset.into());
        match store_changeset(&changeset_id, &parents, &raw_changeset) {
            Ok(_) => {}
            Err(GraftError::NoGraft) => {
                // TODO: ideally this should instead hard-error when not grafting,
                // but NoGraft can theoretically still be emitted in that case.
                debug!("Cannot graft changeset {changeset_id}, not importing");
            }
            Err(GraftError::Ambiguous(candidates)) => die!(
                "Cannot graft {changeset_id}. Candidates: {}",
                itertools::join(candidates.iter(), ", ")
            ),
        }
        previous = (changeset_id, raw_changeset);
    }
    drop(input);
    drop(bundle_writer);
    if !bundle.as_bytes().is_empty() {
        let mut bundle_blob = object_id::default();
        unsafe {
            store_git_blob(&bundle, &mut bundle_blob);
        }
        *BUNDLE_BLOB.lock().unwrap() = Some(bundle_blob);
    }
}

fn branches_for_url(url: Url) -> Vec<Box<BStr>> {
    let mut parts = url.path_segments().unwrap().rev().collect_vec();
    if let Some(Host::Domain(host)) = url.host() {
        parts.push(host);
    }
    let mut branches = parts.into_iter().fold(Vec::<Box<BStr>>::new(), |mut v, p| {
        if !p.is_empty() {
            v.push(
                bstr::join(
                    b"/",
                    Some(p.as_bytes().as_bstr())
                        .into_iter()
                        .chain(v.last().map(|s| &**s)),
                )
                .as_bstr()
                .to_boxed(),
            );
        }
        v
    });
    branches.push(b"metadata".as_bstr().to_boxed());
    branches
}

#[test]
fn test_branches_for_url() {
    assert_eq!(
        branches_for_url(Url::parse("https://server/").unwrap()),
        vec![
            b"server".as_bstr().to_boxed(),
            b"metadata".as_bstr().to_boxed()
        ]
    );
    assert_eq!(
        branches_for_url(Url::parse("https://server:443/").unwrap()),
        vec![
            b"server".as_bstr().to_boxed(),
            b"metadata".as_bstr().to_boxed()
        ]
    );
    assert_eq!(
        branches_for_url(Url::parse("https://server:443/repo").unwrap()),
        vec![
            b"repo".as_bstr().to_boxed(),
            b"server/repo".as_bstr().to_boxed(),
            b"metadata".as_bstr().to_boxed()
        ]
    );
    assert_eq!(
        branches_for_url(Url::parse("https://server:443/dir_a/repo").unwrap()),
        vec![
            b"repo".as_bstr().to_boxed(),
            b"dir_a/repo".as_bstr().to_boxed(),
            b"server/dir_a/repo".as_bstr().to_boxed(),
            b"metadata".as_bstr().to_boxed()
        ]
    );
    assert_eq!(
        branches_for_url(Url::parse("https://server:443/dir_a/dir_b/repo").unwrap()),
        vec![
            b"repo".as_bstr().to_boxed(),
            b"dir_b/repo".as_bstr().to_boxed(),
            b"dir_a/dir_b/repo".as_bstr().to_boxed(),
            b"server/dir_a/dir_b/repo".as_bstr().to_boxed(),
            b"metadata".as_bstr().to_boxed()
        ]
    );
}

pub fn merge_metadata(git_url: Url, hg_url: Option<Url>, branch: Option<&[u8]>) -> bool {
    // Eventually we'll want to handle a full merge, but for now, we only
    // handle the case where we don't have metadata to begin with.
    // The caller should avoid calling this function otherwise.
    assert!(!has_metadata());
    let mut remote_refs = Command::new("git")
        .arg("ls-remote")
        .arg(OsStr::new(git_url.as_ref()))
        .stderr(Stdio::null())
        .output()
        .unwrap()
        .stdout
        .split(|&b| b == b'\n')
        .filter_map(|l| {
            let [sha1, refname] = l.splitn_exact(|&b: &u8| b == b'\t')?;
            Some((
                refname.as_bstr().to_boxed(),
                CommitId::from_bytes(sha1).unwrap(),
            ))
        })
        .collect::<HashMap<_, _>>();
    let mut bundle = if remote_refs.is_empty() && ["http", "https"].contains(&git_url.scheme()) {
        let mut req = HttpRequest::new(git_url.clone());
        req.follow_redirects(true);
        // We let curl handle Content-Encoding: gzip via Accept-Encoding.
        let mut bundle = match req.execute() {
            Ok(bundle) => bundle,
            Err(e) => {
                error!(target: "root", "{}", e);
                return false;
            }
        };
        const BUNDLE_SIGNATURE: &str = "# v2 git bundle\n";
        let signature = (&mut bundle)
            .take(BUNDLE_SIGNATURE.len() as u64)
            .read_all()
            .unwrap();
        if &*signature != BUNDLE_SIGNATURE.as_bytes() {
            error!(target: "root", "Could not find cinnabar metadata");
            return false;
        }
        let mut bundle = BufReader::new(bundle);
        let mut line = Vec::new();
        loop {
            line.truncate(0);
            bundle.read_until(b'\n', &mut line).unwrap();
            if line.ends_with(b"\n") {
                line.pop();
            }
            if line.is_empty() {
                break;
            }
            let [sha1, refname] = line.splitn_exact(b' ').unwrap();
            remote_refs.insert(
                refname.as_bstr().to_boxed(),
                CommitId::from_bytes(sha1).unwrap(),
            );
        }
        Some(bundle)
    } else {
        None
    };

    let branches = branch.map_or_else(
        || hg_url.map(branches_for_url).unwrap_or_default(),
        |b| vec![b.as_bstr().to_boxed()],
    );

    let refname = if let Some(refname) = branches.into_iter().find_map(|branch| {
        if remote_refs.contains_key(&*branch) {
            return Some(branch);
        }
        let cinnabar_refname = bstr::join(b"", [b"refs/cinnabar/".as_bstr(), &branch])
            .as_bstr()
            .to_boxed();
        if remote_refs.contains_key(&cinnabar_refname) {
            return Some(cinnabar_refname);
        }
        let head_refname = bstr::join(b"", [b"refs/heads/".as_bstr(), &branch])
            .as_bstr()
            .to_boxed();
        if remote_refs.contains_key(&head_refname) {
            return Some(head_refname);
        }
        None
    }) {
        refname
    } else {
        error!(target: "root", "Could not find cinnabar metadata");
        return false;
    };

    let mut proc = if let Some(mut bundle) = bundle.as_mut() {
        let mut command = Command::new("git");
        command
            .arg("index-pack")
            .arg("--stdin")
            .arg("--fix-thin")
            .stdout(Stdio::null())
            .stdin(Stdio::piped());
        if progress_enabled() {
            command.arg("-v");
        }
        let mut proc = command.spawn().unwrap();
        let stdin = proc.stdin.as_mut().unwrap();
        copy(&mut bundle, stdin).unwrap();
        proc
    } else {
        let mut command = Command::new("git");
        command
            .arg("fetch")
            .arg("--no-tags")
            .arg("--no-recurse-submodules")
            .arg("-q");
        if progress_enabled() {
            command.arg("--progress");
        } else {
            command.arg("--no-progress");
        }
        command.arg(OsStr::new(git_url.as_ref()));
        command.arg(OsStr::from_bytes(&bstr::join(
            b":",
            [&**refname, b"refs/cinnabar/fetch"],
        )));
        command.spawn().unwrap()
    };
    if !proc.wait().unwrap().success() {
        error!(target: "root", "Failed to fetch cinnabar metadata.");
        return false;
    }

    // Do some basic validation on the metadata we just got.
    let cid = remote_refs.get(&refname).unwrap().clone();
    let commit = RawCommit::read(&cid).unwrap();
    let commit = commit.parse().unwrap();
    if !commit.author().contains_str("cinnabar@git")
        || !String::from_utf8_lossy(commit.body())
            .split_ascii_whitespace()
            .sorted()
            .eq(["files-meta", "unified-manifests-v2"].into_iter())
    {
        error!(target: "root", "Invalid cinnabar metadata.");
        return false;
    }

    // At this point, we'll just assume this is good enough.

    // Get replace refs.
    if commit.tree() != &TreeId::from_str("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap() {
        let mut errors = false;
        let by_sha1 = remote_refs
            .into_iter()
            .map(|(a, b)| (b, a))
            .collect::<BTreeMap<_, _>>();
        let mut needed = Vec::new();
        for item in ls_tree(commit.tree()).unwrap() {
            let cid = CommitId::from_unchecked(item.oid);
            if let Some(refname) = by_sha1.get(&cid) {
                let replace_ref = bstr::join(b"/", [b"refs/cinnabar/replace", &*item.path]);
                needed.push(
                    bstr::join(b":", [&**refname, replace_ref.as_bstr()])
                        .as_bstr()
                        .to_boxed(),
                );
            } else {
                error!(target: "root", "Missing commit: {}", cid);
                errors = true;
            }
        }
        if errors {
            return false;
        }

        if bundle.is_none() {
            let mut command = Command::new("git");
            command
                .arg("fetch")
                .arg("--no-tags")
                .arg("--no-recurse-submodules")
                .arg("-q");
            if progress_enabled() {
                command.arg("--progress");
            } else {
                command.arg("--no-progress");
            }
            command.arg(OsStr::new(git_url.as_ref()));
            command.args(needed.iter().map(|n| OsStr::from_bytes(n)));
            if !command.status().unwrap().success() {
                error!(target: "root", "Failed to fetch cinnabar metadata.");
                return false;
            }
        }
    }

    set_metadata_to(Some(&cid), MetadataFlags::FORCE, "cinnabarclone").unwrap();
    unsafe {
        do_reload();
    }
    true
}
