// Copyright 2017 Pants project contributors (see CONTRIBUTORS.md).
// Licensed under the Apache License, Version 2.0 (see LICENSE).

mod snapshot;
pub use snapshot::{OneOffStoreFileByDigest, Snapshot, StoreFileByDigest, EMPTY_DIGEST,
                   EMPTY_FINGERPRINT};
mod store;
pub use store::Store;
mod pool;
pub use pool::ResettablePool;

extern crate bazel_protos;
extern crate boxfuture;
extern crate byteorder;
extern crate bytes;
extern crate digest;
extern crate futures;
extern crate futures_cpupool;
extern crate glob;
extern crate grpcio;
extern crate hashing;
extern crate hex;
extern crate ignore;
extern crate indexmap;
extern crate itertools;
#[macro_use]
extern crate lazy_static;
extern crate lmdb;
#[macro_use]
extern crate log;
#[cfg(test)]
extern crate mock;
extern crate protobuf;
extern crate resettable;
extern crate sha2;
extern crate tempfile;
#[cfg(test)]
extern crate testutil;

use std::cmp::min;
use std::collections::HashSet;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::{fmt, fs};

use bytes::Bytes;
use futures::future::{self, Future};
use glob::Pattern;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use indexmap::{IndexMap, IndexSet, map::Entry::Occupied};

use boxfuture::{BoxFuture, Boxable};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Stat {
  Link(Link),
  Dir(Dir),
  File(File),
}

impl Stat {
  pub fn path(&self) -> &Path {
    match self {
      &Stat::Dir(Dir(ref p)) => p.as_path(),
      &Stat::File(File { path: ref p, .. }) => p.as_path(),
      &Stat::Link(Link(ref p)) => p.as_path(),
    }
  }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Link(pub PathBuf);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Dir(pub PathBuf);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct File {
  pub path: PathBuf,
  pub is_executable: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum PathStat {
  Dir {
    // The symbolic name of some filesystem Path, which is context specific.
    path: PathBuf,
    // The canonical Stat that underlies the Path.
    stat: Dir,
  },
  File {
    // The symbolic name of some filesystem Path, which is context specific.
    path: PathBuf,
    // The canonical Stat that underlies the Path.
    stat: File,
  },
}

impl PathStat {
  pub fn dir(path: PathBuf, stat: Dir) -> PathStat {
    PathStat::Dir {
      path: path,
      stat: stat,
    }
  }

  pub fn file(path: PathBuf, stat: File) -> PathStat {
    PathStat::File {
      path: path,
      stat: stat,
    }
  }

  pub fn path(&self) -> &Path {
    match self {
      &PathStat::Dir { ref path, .. } => path.as_path(),
      &PathStat::File { ref path, .. } => path.as_path(),
    }
  }
}

#[derive(Debug)]
pub struct GitignoreStyleExcludes {
  patterns: Vec<String>,
  gitignore: Gitignore,
}

impl GitignoreStyleExcludes {
  fn create(patterns: &[String]) -> Result<Arc<Self>, String> {
    if patterns.is_empty() {
      return Ok(EMPTY_IGNORE.clone());
    }

    let gitignore = Self::create_gitignore(patterns)
      .map_err(|e| format!("Could not parse glob excludes {:?}: {:?}", patterns, e))?;

    Ok(Arc::new(Self {
      patterns: patterns.to_vec(),
      gitignore,
    }))
  }

  fn create_gitignore(patterns: &[String]) -> Result<Gitignore, ignore::Error> {
    let mut ignore_builder = GitignoreBuilder::new("");
    for pattern in patterns {
      ignore_builder.add_line(None, pattern.as_str())?;
    }
    ignore_builder.build()
  }

  fn exclude_patterns(&self) -> &[String] {
    self.patterns.as_slice()
  }

  fn is_ignored(&self, stat: &Stat) -> bool {
    let is_dir = match stat {
      &Stat::Dir(_) => true,
      _ => false,
    };
    match self.gitignore.matched(stat.path(), is_dir) {
      ignore::Match::None | ignore::Match::Whitelist(_) => false,
      ignore::Match::Ignore(_) => true,
    }
  }
}

lazy_static! {
  static ref PARENT_DIR: &'static str = "..";
  static ref SINGLE_STAR_GLOB: Pattern = Pattern::new("*").unwrap();
  static ref DOUBLE_STAR: &'static str = "**";
  static ref DOUBLE_STAR_GLOB: Pattern = Pattern::new(*DOUBLE_STAR).unwrap();
  static ref EMPTY_IGNORE: Arc<GitignoreStyleExcludes> = Arc::new(GitignoreStyleExcludes {
    patterns: vec![],
    gitignore: Gitignore::empty(),
  });
  static ref MISSING_GLOB_SOURCE: GlobParsedSource = GlobParsedSource(String::from(""));
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum PathGlob {
  Wildcard {
    canonical_dir: Dir,
    symbolic_path: PathBuf,
    wildcard: Pattern,
  },
  DirWildcard {
    canonical_dir: Dir,
    symbolic_path: PathBuf,
    wildcard: Pattern,
    remainder: Vec<Pattern>,
  },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct GlobParsedSource(String);

#[derive(Clone, Debug)]
pub struct PathGlobIncludeEntry {
  pub input: GlobParsedSource,
  pub globs: Vec<PathGlob>,
}

impl PathGlobIncludeEntry {
  fn to_sourced_globs(&self) -> Vec<GlobWithSource> {
    self
      .globs
      .clone()
      .into_iter()
      .map(|path_glob| GlobWithSource {
        path_glob,
        source: GlobSource::ParsedInput(self.input.clone()),
      })
      .collect()
  }
}

impl PathGlob {
  fn wildcard(canonical_dir: Dir, symbolic_path: PathBuf, wildcard: Pattern) -> PathGlob {
    PathGlob::Wildcard {
      canonical_dir: canonical_dir,
      symbolic_path: symbolic_path,
      wildcard: wildcard,
    }
  }

  fn dir_wildcard(
    canonical_dir: Dir,
    symbolic_path: PathBuf,
    wildcard: Pattern,
    remainder: Vec<Pattern>,
  ) -> PathGlob {
    PathGlob::DirWildcard {
      canonical_dir: canonical_dir,
      symbolic_path: symbolic_path,
      wildcard: wildcard,
      remainder: remainder,
    }
  }

  pub fn create(filespecs: &[String]) -> Result<Vec<PathGlob>, String> {
    // Getting a Vec<PathGlob> per filespec is needed to create a `PathGlobs`, but we don't need
    // that here.
    let filespecs_globs = Self::spread_filespecs(filespecs)?;
    let all_globs = Self::flatten_entries(filespecs_globs);
    Ok(all_globs)
  }

  fn flatten_entries(entries: Vec<PathGlobIncludeEntry>) -> Vec<PathGlob> {
    entries.into_iter().flat_map(|entry| entry.globs).collect()
  }

  fn spread_filespecs(filespecs: &[String]) -> Result<Vec<PathGlobIncludeEntry>, String> {
    let mut spec_globs_map = Vec::new();
    for filespec in filespecs {
      let canonical_dir = Dir(PathBuf::new());
      let symbolic_path = PathBuf::new();
      spec_globs_map.push(PathGlobIncludeEntry {
        input: GlobParsedSource(filespec.clone()),
        globs: PathGlob::parse(canonical_dir, symbolic_path, filespec)?,
      });
    }
    Ok(spec_globs_map)
  }

  ///
  /// Given a filespec String relative to a canonical Dir and path, split it into path components
  /// while eliminating consecutive '**'s (to avoid repetitive traversing), and parse it to a
  /// series of PathGlob objects.
  ///
  fn parse(
    canonical_dir: Dir,
    symbolic_path: PathBuf,
    filespec: &str,
  ) -> Result<Vec<PathGlob>, String> {
    let mut parts = Vec::new();
    let mut prev_was_doublestar = false;
    for component in Path::new(filespec).components() {
      let part = match component {
        Component::Prefix(..) | Component::RootDir => {
          return Err(format!("Absolute paths not supported: {:?}", filespec))
        }
        Component::CurDir => continue,
        c => c.as_os_str(),
      };

      // Ignore repeated doublestar instances.
      let cur_is_doublestar = *DOUBLE_STAR == part;
      if prev_was_doublestar && cur_is_doublestar {
        continue;
      }
      prev_was_doublestar = cur_is_doublestar;

      // NB: Because the filespec is a String input, calls to `to_str_lossy` are not lossy; the
      // use of `Path` is strictly for os-independent Path parsing.
      parts.push(Pattern::new(&part.to_string_lossy())
        .map_err(|e| format!("Could not parse {:?} as a glob: {:?}", filespec, e))?);
    }

    PathGlob::parse_globs(canonical_dir, symbolic_path, &parts)
  }

  ///
  /// Given a filespec as Patterns, create a series of PathGlob objects.
  ///
  fn parse_globs(
    canonical_dir: Dir,
    symbolic_path: PathBuf,
    parts: &[Pattern],
  ) -> Result<Vec<PathGlob>, String> {
    if parts.is_empty() {
      Ok(vec![])
    } else if *DOUBLE_STAR == parts[0].as_str() {
      if parts.len() == 1 {
        // Per https://git-scm.com/docs/gitignore:
        //  "A trailing '/**' matches everything inside. For example, 'abc/**' matches all files
        //  inside directory "abc", relative to the location of the .gitignore file, with infinite
        //  depth."
        return Ok(vec![
          PathGlob::dir_wildcard(
            canonical_dir.clone(),
            symbolic_path.clone(),
            SINGLE_STAR_GLOB.clone(),
            vec![DOUBLE_STAR_GLOB.clone()],
          ),
          PathGlob::wildcard(canonical_dir, symbolic_path, SINGLE_STAR_GLOB.clone()),
        ]);
      }

      // There is a double-wildcard in a dirname of the path: double wildcards are recursive,
      // so there are two remainder possibilities: one with the double wildcard included, and the
      // other without.
      let pathglob_with_doublestar = PathGlob::dir_wildcard(
        canonical_dir.clone(),
        symbolic_path.clone(),
        SINGLE_STAR_GLOB.clone(),
        parts[0..].to_vec(),
      );
      let pathglob_no_doublestar = if parts.len() == 2 {
        PathGlob::wildcard(canonical_dir, symbolic_path, parts[1].clone())
      } else {
        PathGlob::dir_wildcard(
          canonical_dir,
          symbolic_path,
          parts[1].clone(),
          parts[2..].to_vec(),
        )
      };
      Ok(vec![pathglob_with_doublestar, pathglob_no_doublestar])
    } else if *PARENT_DIR == parts[0].as_str() {
      // A request for the parent of `canonical_dir`: since we've already expanded the directory
      // to make it canonical, we can safely drop it directly and recurse without this component.
      // The resulting symbolic path will continue to contain a literal `..`.
      let mut canonical_dir_parent = canonical_dir;
      let mut symbolic_path_parent = symbolic_path;
      if !canonical_dir_parent.0.pop() {
        return Err(format!(
          "Globs may not traverse outside the root: {:?}",
          parts
        ));
      }
      symbolic_path_parent.push(Path::new(*PARENT_DIR));
      PathGlob::parse_globs(canonical_dir_parent, symbolic_path_parent, &parts[1..])
    } else if parts.len() == 1 {
      // This is the path basename.
      Ok(vec![
        PathGlob::wildcard(canonical_dir, symbolic_path, parts[0].clone()),
      ])
    } else {
      // This is a path dirname.
      Ok(vec![
        PathGlob::dir_wildcard(
          canonical_dir,
          symbolic_path,
          parts[0].clone(),
          parts[1..].to_vec(),
        ),
      ])
    }
  }
}

#[derive(Debug)]
pub enum StrictGlobMatching {
  Error,
  Warn,
  Ignore,
}

impl StrictGlobMatching {
  // TODO(cosmicexplorer): match this up with the allowed values for the GlobMatchErrorBehavior type
  // in python somehow?
  pub fn create(behavior: &str) -> Result<Self, String> {
    match behavior {
      "ignore" => Ok(StrictGlobMatching::Ignore),
      "warn" => Ok(StrictGlobMatching::Warn),
      "error" => Ok(StrictGlobMatching::Error),
      _ => Err(format!(
        "Unrecognized strict glob matching behavior: {}.",
        behavior,
      )),
    }
  }

  pub fn should_check_glob_matches(&self) -> bool {
    match self {
      &StrictGlobMatching::Ignore => false,
      _ => true,
    }
  }

  pub fn should_throw_on_error(&self) -> bool {
    match self {
      &StrictGlobMatching::Error => true,
      _ => false,
    }
  }
}

#[derive(Debug)]
pub struct PathGlobs {
  include: Vec<PathGlobIncludeEntry>,
  exclude: Arc<GitignoreStyleExcludes>,
  strict_match_behavior: StrictGlobMatching,
}

impl PathGlobs {
  pub fn create(
    include: &[String],
    exclude: &[String],
    strict_match_behavior: StrictGlobMatching,
  ) -> Result<PathGlobs, String> {
    let include = PathGlob::spread_filespecs(include)?;
    Self::create_with_globs_and_match_behavior(include, exclude, strict_match_behavior)
  }

  fn create_with_globs_and_match_behavior(
    include: Vec<PathGlobIncludeEntry>,
    exclude: &[String],
    strict_match_behavior: StrictGlobMatching,
  ) -> Result<PathGlobs, String> {
    let gitignore_excludes = GitignoreStyleExcludes::create(exclude)?;
    Ok(PathGlobs {
      include,
      exclude: gitignore_excludes,
      strict_match_behavior,
    })
  }

  pub fn from_globs(include: Vec<PathGlob>) -> Result<PathGlobs, String> {
    let include = include
      .into_iter()
      .map(|glob| PathGlobIncludeEntry {
        input: MISSING_GLOB_SOURCE.clone(),
        globs: vec![glob],
      })
      .collect();
    // An empty exclude becomes EMPTY_IGNORE.
    PathGlobs::create_with_globs_and_match_behavior(include, &vec![], StrictGlobMatching::Ignore)
  }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum GlobSource {
  ParsedInput(GlobParsedSource),
  ParentGlob(PathGlob),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct GlobWithSource {
  path_glob: PathGlob,
  source: GlobSource,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum GlobMatch {
  SuccessfullyMatchedSomeFiles,
  DidNotMatchAnyFiles,
}

#[derive(Debug)]
struct GlobExpansionCacheEntry {
  globs: Vec<PathGlob>,
  matched: GlobMatch,
  sources: Vec<GlobSource>,
}

// FIXME(#5871): move glob matching to its own file so we don't need to leak this object (through
// the return type of expand_single()).
#[derive(Debug)]
pub struct SingleExpansionResult {
  sourced_glob: GlobWithSource,
  path_stats: Vec<PathStat>,
  globs: Vec<PathGlob>,
}

#[derive(Debug)]
struct PathGlobsExpansion<T: Sized> {
  context: T,
  // Globs that have yet to be expanded, in order.
  todo: Vec<GlobWithSource>,
  // Paths to exclude.
  exclude: Arc<GitignoreStyleExcludes>,
  // Globs that have already been expanded.
  completed: IndexMap<PathGlob, GlobExpansionCacheEntry>,
  // Unique Paths that have been matched, in order.
  outputs: IndexSet<PathStat>,
}

///
/// All Stats consumed or return by this type are relative to the root.
///
pub struct PosixFS {
  root: Dir,
  pool: Arc<ResettablePool>,
  ignore: Arc<GitignoreStyleExcludes>,
}

impl PosixFS {
  pub fn new<P: AsRef<Path>>(
    root: P,
    pool: Arc<ResettablePool>,
    ignore_patterns: Vec<String>,
  ) -> Result<PosixFS, String> {
    let root: &Path = root.as_ref();
    let canonical_root = root
      .canonicalize()
      .and_then(|canonical| {
        canonical.metadata().and_then(|metadata| {
          if metadata.is_dir() {
            Ok(Dir(canonical))
          } else {
            Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "Not a directory.",
            ))
          }
        })
      })
      .map_err(|e| format!("Could not canonicalize root {:?}: {:?}", root, e))?;

    let ignore = GitignoreStyleExcludes::create(&ignore_patterns).map_err(|e| {
      format!(
        "Could not parse build ignore inputs {:?}: {:?}",
        ignore_patterns, e
      )
    })?;
    Ok(PosixFS {
      root: canonical_root,
      pool: pool,
      ignore: ignore,
    })
  }

  fn scandir_sync(root: PathBuf, dir_relative_to_root: Dir) -> Result<Vec<Stat>, io::Error> {
    let dir_abs = root.join(&dir_relative_to_root.0);
    let mut stats: Vec<Stat> = dir_abs
      .read_dir()?
      .map(|readdir| {
        let dir_entry = readdir?;
        let get_metadata = || std::fs::metadata(dir_abs.join(dir_entry.file_name()));
        PosixFS::stat_internal(
          dir_relative_to_root.0.join(dir_entry.file_name()),
          dir_entry.file_type()?,
          &dir_abs,
          get_metadata,
        )
      })
      .collect::<Result<Vec<_>, io::Error>>()?;
    stats.sort_by(|s1, s2| s1.path().cmp(s2.path()));
    Ok(stats)
  }

  pub fn is_ignored(&self, stat: &Stat) -> bool {
    self.ignore.is_ignored(stat)
  }

  pub fn read_file(&self, file: &File) -> BoxFuture<FileContent, io::Error> {
    let path = file.path.clone();
    let path_abs = self.root.0.join(&file.path);
    self
      .pool
      .spawn_fn(move || {
        std::fs::File::open(&path_abs).and_then(|mut f| {
          let mut content = Vec::new();
          f.read_to_end(&mut content)?;
          Ok(FileContent {
            path: path,
            content: Bytes::from(content),
          })
        })
      })
      .to_boxed()
  }

  pub fn read_link(&self, link: &Link) -> BoxFuture<PathBuf, io::Error> {
    let link_parent = link.0.parent().map(|p| p.to_owned());
    let link_abs = self.root.0.join(link.0.as_path()).to_owned();
    self
      .pool
      .spawn_fn(move || {
        link_abs.read_link().and_then(|path_buf| {
          if path_buf.is_absolute() {
            Err(io::Error::new(
              io::ErrorKind::InvalidData,
              format!("Absolute symlink: {:?}", link_abs),
            ))
          } else {
            link_parent
              .map(|parent| parent.join(path_buf))
              .ok_or_else(|| {
                io::Error::new(
                  io::ErrorKind::InvalidData,
                  format!("Symlink without a parent?: {:?}", link_abs),
                )
              })
          }
        })
      })
      .to_boxed()
  }

  ///
  /// Makes a Stat for path_for_stat relative to absolute_path_to_root.
  ///
  fn stat_internal<F>(
    path_for_stat: PathBuf,
    file_type: std::fs::FileType,
    absolute_path_to_root: &Path,
    get_metadata: F,
  ) -> Result<Stat, io::Error>
  where
    F: FnOnce() -> Result<fs::Metadata, io::Error>,
  {
    if !path_for_stat.is_relative() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "Argument path_for_stat to PosixFS::stat must be relative path, got {:?}",
          path_for_stat
        ),
      ));
    }
    // TODO: Make this an instance method, and stop having to check this every call.
    if !absolute_path_to_root.is_absolute() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "Argument absolute_path_to_root to PosixFS::stat must be absolute path, got {:?}",
          absolute_path_to_root
        ),
      ));
    }
    if file_type.is_dir() {
      Ok(Stat::Dir(Dir(path_for_stat)))
    } else if file_type.is_file() {
      let is_executable = get_metadata()?.permissions().mode() & 0o100 == 0o100;
      Ok(Stat::File(File {
        path: path_for_stat,
        is_executable: is_executable,
      }))
    } else if file_type.is_symlink() {
      Ok(Stat::Link(Link(path_for_stat)))
    } else {
      Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
          "Expected File, Dir or Link, but {:?} (relative to {:?}) was a {:?}",
          path_for_stat, absolute_path_to_root, file_type
        ),
      ))
    }
  }

  pub fn stat(&self, relative_path: PathBuf) -> Result<Stat, io::Error> {
    PosixFS::stat_path(relative_path, &self.root.0)
  }

  fn stat_path(relative_path: PathBuf, root: &Path) -> Result<Stat, io::Error> {
    let metadata = fs::symlink_metadata(root.join(&relative_path))?;
    PosixFS::stat_internal(relative_path, metadata.file_type(), &root, || Ok(metadata))
  }

  pub fn scandir(&self, dir: &Dir) -> BoxFuture<Vec<Stat>, io::Error> {
    let dir = dir.to_owned();
    let root = self.root.0.clone();
    self
      .pool
      .spawn_fn(move || PosixFS::scandir_sync(root, dir))
      .to_boxed()
  }
}

impl VFS<io::Error> for Arc<PosixFS> {
  fn read_link(&self, link: Link) -> BoxFuture<PathBuf, io::Error> {
    PosixFS::read_link(self, &link)
  }

  fn scandir(&self, dir: Dir) -> BoxFuture<Vec<Stat>, io::Error> {
    PosixFS::scandir(self, &dir)
  }

  fn is_ignored(&self, stat: &Stat) -> bool {
    PosixFS::is_ignored(self, stat)
  }

  fn mk_error(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
  }
}

pub trait PathStatGetter<E> {
  fn path_stats(&self, paths: Vec<PathBuf>) -> BoxFuture<Vec<Option<PathStat>>, E>;
}

impl PathStatGetter<io::Error> for Arc<PosixFS> {
  fn path_stats(&self, paths: Vec<PathBuf>) -> BoxFuture<Vec<Option<PathStat>>, io::Error> {
    future::join_all(
      paths
        .into_iter()
        .map(|path| {
          let root = self.root.0.clone();
          let fs = self.clone();
          self
            .pool
            .spawn_fn(move || PosixFS::stat_path(path, &root))
            .then(|stat_result| match stat_result {
              Ok(v) => Ok(Some(v)),
              Err(err) => match err.kind() {
                io::ErrorKind::NotFound => Ok(None),
                _ => Err(err),
              },
            })
            .and_then(move |maybe_stat| {
              match maybe_stat {
                // Note: This will drop PathStats for symlinks which don't point anywhere.
                Some(Stat::Link(link)) => fs.canonicalize(link.0.clone(), link),
                Some(Stat::Dir(dir)) => {
                  future::ok(Some(PathStat::dir(dir.0.clone(), dir))).to_boxed()
                }
                Some(Stat::File(file)) => {
                  future::ok(Some(PathStat::file(file.path.clone(), file))).to_boxed()
                }
                None => future::ok(None).to_boxed(),
              }
            })
        })
        .collect::<Vec<_>>(),
    ).to_boxed()
  }
}

///
/// A context for filesystem operations parameterized on an error type 'E'.
///
pub trait VFS<E: Send + Sync + 'static>: Clone + Send + Sync + 'static {
  fn read_link(&self, link: Link) -> BoxFuture<PathBuf, E>;
  fn scandir(&self, dir: Dir) -> BoxFuture<Vec<Stat>, E>;
  fn is_ignored(&self, stat: &Stat) -> bool;
  fn mk_error(msg: &str) -> E;

  ///
  /// Canonicalize the Link for the given Path to an underlying File or Dir. May result
  /// in None if the PathStat represents a broken Link.
  ///
  /// Skips ignored paths both before and after expansion.
  ///
  /// TODO: Should handle symlink loops (which would exhibit as an infinite loop in expand).
  ///
  fn canonicalize(&self, symbolic_path: PathBuf, link: Link) -> BoxFuture<Option<PathStat>, E> {
    // Read the link, which may result in PathGlob(s) that match 0 or 1 Path.
    let context = self.clone();
    self
      .read_link(link)
      .map(|dest_path| {
        // If the link destination can't be parsed as PathGlob(s), it is broken.
        dest_path
          .to_str()
          .and_then(|dest_str| {
            // Escape any globs in the parsed dest, which should guarantee one output PathGlob.
            PathGlob::create(&[Pattern::escape(dest_str)]).ok()
          })
          .unwrap_or_else(|| vec![])
      })
      .and_then(|link_globs| {
        let new_path_globs =
          future::result(PathGlobs::from_globs(link_globs)).map_err(|e| Self::mk_error(e.as_str()));
        new_path_globs.and_then(move |path_globs| context.expand(path_globs))
      })
      .map(|mut path_stats| {
        // Since we've escaped any globs in the parsed path, expect either 0 or 1 destination.
        path_stats.pop().map(|ps| match ps {
          PathStat::Dir { stat, .. } => PathStat::dir(symbolic_path, stat),
          PathStat::File { stat, .. } => PathStat::file(symbolic_path, stat),
        })
      })
      .to_boxed()
  }

  fn directory_listing(
    &self,
    canonical_dir: Dir,
    symbolic_path: PathBuf,
    wildcard: Pattern,
    exclude: &Arc<GitignoreStyleExcludes>,
  ) -> BoxFuture<Vec<PathStat>, E> {
    // List the directory.
    let context = self.clone();
    let exclude = exclude.clone();

    self
      .scandir(canonical_dir)
      .and_then(move |dir_listing| {
        // Match any relevant Stats, and join them into PathStats.
        future::join_all(
          dir_listing
            .into_iter()
            .filter(|stat| {
              // Match relevant filenames.
              stat
                .path()
                .file_name()
                .map(|file_name| wildcard.matches_path(Path::new(file_name)))
                .unwrap_or(false)
            })
            .filter_map(|stat| {
              // Append matched filenames.
              stat
                .path()
                .file_name()
                .map(|file_name| symbolic_path.join(file_name))
                .map(|symbolic_stat_path| (symbolic_stat_path, stat))
            })
            .map(|(stat_symbolic_path, stat)| {
              // Canonicalize matched PathStats, and filter paths that are ignored by either the
              // context, or by local excludes. Note that we apply context ignore patterns to both
              // the symbolic and canonical names of Links, but only apply local excludes to their
              // symbolic names.
              if context.is_ignored(&stat) || exclude.is_ignored(&stat) {
                future::ok(None).to_boxed()
              } else {
                match stat {
                  Stat::Link(l) => context.canonicalize(stat_symbolic_path, l),
                  Stat::Dir(d) => {
                    future::ok(Some(PathStat::dir(stat_symbolic_path.to_owned(), d))).to_boxed()
                  }
                  Stat::File(f) => {
                    future::ok(Some(PathStat::file(stat_symbolic_path.to_owned(), f))).to_boxed()
                  }
                }
              }
            })
            .collect::<Vec<_>>(),
        )
      })
      .map(|path_stats| {
        // See the TODO above.
        path_stats.into_iter().filter_map(|pso| pso).collect()
      })
      .to_boxed()
  }

  ///
  /// Recursively expands PathGlobs into PathStats while applying excludes.
  ///
  fn expand(&self, path_globs: PathGlobs) -> BoxFuture<Vec<PathStat>, E> {
    let PathGlobs {
      include,
      exclude,
      strict_match_behavior,
    } = path_globs;

    if include.is_empty() {
      return future::ok(vec![]).to_boxed();
    }

    let init = PathGlobsExpansion {
      context: self.clone(),
      todo: include
        .iter()
        .flat_map(|entry| entry.to_sourced_globs())
        .collect(),
      exclude,
      completed: IndexMap::default(),
      outputs: IndexSet::default(),
    };
    future::loop_fn(init, |mut expansion| {
      // Request the expansion of all outstanding PathGlobs as a batch.
      let round = future::join_all({
        let exclude = &expansion.exclude;
        let context = &expansion.context;
        expansion
          .todo
          .drain(..)
          .map(|sourced_glob| context.expand_single(sourced_glob, exclude))
          .collect::<Vec<_>>()
      });
      round.map(move |single_expansion_results| {
        // Collect distinct new PathStats and PathGlobs
        for exp in single_expansion_results {
          let SingleExpansionResult {
            sourced_glob: GlobWithSource { path_glob, source },
            path_stats,
            globs,
          } = exp;

          expansion.outputs.extend(path_stats.clone());

          expansion
            .completed
            .entry(path_glob.clone())
            .or_insert_with(|| GlobExpansionCacheEntry {
              globs: globs.clone(),
              matched: if path_stats.is_empty() {
                GlobMatch::DidNotMatchAnyFiles
              } else {
                GlobMatch::SuccessfullyMatchedSomeFiles
              },
              sources: vec![],
            })
            .sources
            .push(source);

          // Do we need to worry about cloning for all these `GlobSource`s (each containing a
          // `PathGlob`)?
          let source_for_children = GlobSource::ParentGlob(path_glob);
          for child_glob in globs {
            if let Occupied(mut entry) = expansion.completed.entry(child_glob.clone()) {
              entry.get_mut().sources.push(source_for_children.clone());
            } else {
              expansion.todo.push(GlobWithSource {
                path_glob: child_glob,
                source: source_for_children.clone(),
              });
            }
          }
        }

        // If there were any new PathGlobs, continue the expansion.
        if expansion.todo.is_empty() {
          future::Loop::Break(expansion)
        } else {
          future::Loop::Continue(expansion)
        }
      })
    }).and_then(move |final_expansion| {
      // Finally, capture the resulting PathStats from the expansion.
      let PathGlobsExpansion {
        outputs,
        mut completed,
        exclude,
        ..
      } = final_expansion;

      let match_results: Vec<_> = outputs.into_iter().collect();

      if strict_match_behavior.should_check_glob_matches() {
        // Each `GlobExpansionCacheEntry` stored in `completed` for some `PathGlob` has the field
        // `matched` to denote whether that specific `PathGlob` matched any files. We propagate a
        // positive `matched` condition to all transitive "parents" of any glob which expands to
        // some non-empty set of `PathStat`s. The `sources` field contains the parents (see the enum
        // `GlobSource`), which may be another glob, or it might be a `GlobParsedSource`. We record
        // all `GlobParsedSource` inputs which transitively expanded to some file here, and below we
        // warn or error if some of the inputs were not found.
        let mut inputs_with_matches: HashSet<GlobParsedSource> = HashSet::new();

        // `completed` is an IndexMap, and we immediately insert every glob we expand into
        // `completed`, recording any `PathStat`s and `PathGlob`s it expanded to (and then expanding
        // those child globs in the next iteration of the loop_fn). If we iterate in
        // reverse order of expansion (using .rev()), we ensure that we have already visited every
        // "child" glob of the glob we are operating on while iterating. This is a reverse
        // "topological ordering" which preserves the partial order from parent to child globs.
        let all_globs: Vec<PathGlob> = completed.keys().rev().map(|pg| pg.clone()).collect();
        for cur_glob in all_globs {
          // Note that we talk of "parents" and "childen", but this structure is actually a DAG,
          // because different `DirWildcard`s can potentially expand (transitively) to the same
          // intermediate glob. The "parents" of each glob are stored in the `sources` field of its
          // `GlobExpansionCacheEntry` (which is mutably updated with any new parents on each
          // iteration of the loop_fn above). This can be considered "amortized" and/or "memoized".
          let new_matched_source_globs = match completed.get(&cur_glob).unwrap() {
            &GlobExpansionCacheEntry {
              ref matched,
              ref sources,
              ..
            } => match matched {
              // Neither this glob, nor any of its children, expanded to any `PathStat`s, so we have
              // nothing to propagate.
              &GlobMatch::DidNotMatchAnyFiles => vec![],
              &GlobMatch::SuccessfullyMatchedSomeFiles => sources
                .iter()
                .filter_map(|src| match src {
                  // This glob matched some files, so its parent also matched some files.
                  &GlobSource::ParentGlob(ref path_glob) => Some(path_glob.clone()),
                  // We've found one of the root inputs, coming from a glob which transitively
                  // matched some child -- record it (this may already exist in the set).
                  &GlobSource::ParsedInput(ref parsed_source) => {
                    inputs_with_matches.insert(parsed_source.clone());
                    None
                  }
                })
                .collect(),
            },
          };
          new_matched_source_globs.into_iter().for_each(|path_glob| {
            // Overwrite whatever was in there before -- we now know these globs transitively
            // expanded to some non-empty set of `PathStat`s.
            let entry = completed.get_mut(&path_glob).unwrap();
            entry.matched = GlobMatch::SuccessfullyMatchedSomeFiles;
          });
        }

        // Get all the inputs which didn't transitively expand to any files.
        let non_matching_inputs: Vec<GlobParsedSource> = include
          .into_iter()
          .map(|entry| entry.input)
          .filter(|parsed_source| !inputs_with_matches.contains(parsed_source))
          .collect();

        if !non_matching_inputs.is_empty() {
          // TODO(#5684): explain what global and/or target-specific option to set to
          // modify this behavior!
          let msg = format!(
            "Globs did not match. Excludes were: {:?}. Unmatched globs were: {:?}.",
            exclude.exclude_patterns(),
            non_matching_inputs
              .iter()
              .map(|parsed_source| parsed_source.0.clone())
              .collect::<Vec<_>>(),
          );
          if strict_match_behavior.should_throw_on_error() {
            return future::err(Self::mk_error(&msg));
          } else {
            // FIXME(#5683): warn!() doesn't seem to do anything?
            // TODO(#5683): this doesn't have any useful context (the stack trace) without
            // being thrown -- this needs to be provided, otherwise this is unusable.
            warn!("{}", msg);
          }
        }
      }

      future::ok(match_results)
    })
      .to_boxed()
  }

  ///
  /// Apply a PathGlob, returning PathStats and additional PathGlobs that are needed for the
  /// expansion.
  ///
  fn expand_single(
    &self,
    sourced_glob: GlobWithSource,
    exclude: &Arc<GitignoreStyleExcludes>,
  ) -> BoxFuture<SingleExpansionResult, E> {
    match sourced_glob.path_glob.clone() {
      PathGlob::Wildcard { canonical_dir, symbolic_path, wildcard } =>
        // Filter directory listing to return PathStats, with no continuation.
        self.directory_listing(canonical_dir, symbolic_path, wildcard, exclude)
        .map(move |path_stats| SingleExpansionResult {
          sourced_glob,
          path_stats,
          globs: vec![],
        })
        .to_boxed(),
      PathGlob::DirWildcard { canonical_dir, symbolic_path, wildcard, remainder } =>
        // Filter directory listing and request additional PathGlobs for matched Dirs.
        self.directory_listing(canonical_dir, symbolic_path, wildcard, exclude)
          .and_then(move |path_stats| {
            path_stats.into_iter()
              .filter_map(|ps| match ps {
                PathStat::Dir { path, stat } =>
                  Some(
                    PathGlob::parse_globs(stat, path, &remainder)
                      .map_err(|e| Self::mk_error(e.as_str()))
                  ),
                PathStat::File { .. } => None,
              })
              .collect::<Result<Vec<_>, E>>()
          })
          .map(move |path_globs| {
            let flattened = path_globs
              .into_iter()
              .flat_map(|path_globs| path_globs.into_iter())
              .collect();
            SingleExpansionResult {
              sourced_glob,
              path_stats: vec![],
              globs: flattened,
            }
          })
          .to_boxed(),
    }
  }
}

pub struct FileContent {
  pub path: PathBuf,
  pub content: Bytes,
}

impl fmt::Debug for FileContent {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    let len = min(self.content.len(), 5);
    let describer = if len < self.content.len() {
      "starting "
    } else {
      ""
    };
    write!(
      f,
      "FileContent(path={:?}, content={} bytes {}{:?})",
      self.path,
      self.content.len(),
      describer,
      &self.content[..len]
    )
  }
}

// Like std::fs::create_dir_all, except handles concurrent calls among multiple
// threads or processes. Originally lifted from rustc.
pub fn safe_create_dir_all_ioerror(path: &Path) -> Result<(), io::Error> {
  match fs::create_dir(path) {
    Ok(()) => return Ok(()),
    Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
    Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
    Err(e) => return Err(e),
  }
  match path.parent() {
    Some(p) => try!(safe_create_dir_all_ioerror(p)),
    None => return Ok(()),
  }
  match fs::create_dir(path) {
    Ok(()) => Ok(()),
    Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
    Err(e) => Err(e),
  }
}

fn safe_create_dir_all(path: &Path) -> Result<(), String> {
  safe_create_dir_all_ioerror(path)
    .map_err(|e| format!("Failed to create dir {:?} due to {:?}", path, e))
}

#[cfg(test)]
mod posixfs_test {
  extern crate tempfile;
  extern crate testutil;

  use self::testutil::make_file;
  use super::{Dir, File, Link, PathStat, PathStatGetter, PosixFS, ResettablePool, Stat};
  use futures::Future;
  use std;
  use std::path::{Path, PathBuf};
  use std::sync::Arc;

  #[test]
  fn is_executable_false() {
    let dir = tempfile::TempDir::new().unwrap();
    make_file(&dir.path().join("marmosets"), &[], 0o611);
    assert_only_file_is_executable(dir.path(), false);
  }

  #[test]
  fn is_executable_true() {
    let dir = tempfile::TempDir::new().unwrap();
    make_file(&dir.path().join("photograph_marmosets"), &[], 0o700);
    assert_only_file_is_executable(dir.path(), true);
  }

  #[test]
  fn read_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = PathBuf::from("marmosets");
    let content = "cute".as_bytes().to_vec();
    make_file(
      &std::fs::canonicalize(dir.path()).unwrap().join(&path),
      &content,
      0o600,
    );
    let fs = new_posixfs(&dir.path());
    let file_content = fs.read_file(&File {
      path: path.clone(),
      is_executable: false,
    }).wait()
      .unwrap();
    assert_eq!(file_content.path, path);
    assert_eq!(file_content.content, content);
  }

  #[test]
  fn read_file_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    new_posixfs(&dir.path())
      .read_file(&File {
        path: PathBuf::from("marmosets"),
        is_executable: false,
      })
      .wait()
      .expect_err("Expected error");
  }

  #[test]
  fn stat_executable_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    let path = PathBuf::from("photograph_marmosets");
    make_file(&dir.path().join(&path), &[], 0o700);
    assert_eq!(
      posix_fs.stat(path.clone()).unwrap(),
      super::Stat::File(File {
        path: path,
        is_executable: true,
      })
    )
  }

  #[test]
  fn stat_nonexecutable_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    let path = PathBuf::from("marmosets");
    make_file(&dir.path().join(&path), &[], 0o600);
    assert_eq!(
      posix_fs.stat(path.clone()).unwrap(),
      super::Stat::File(File {
        path: path,
        is_executable: false,
      })
    )
  }

  #[test]
  fn stat_dir() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    let path = PathBuf::from("enclosure");
    std::fs::create_dir(dir.path().join(&path)).unwrap();
    assert_eq!(
      posix_fs.stat(path.clone()).unwrap(),
      super::Stat::Dir(Dir(path))
    )
  }

  #[test]
  fn stat_symlink() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    let path = PathBuf::from("marmosets");
    make_file(&dir.path().join(&path), &[], 0o600);

    let link_path = PathBuf::from("remarkably_similar_marmoset");
    std::os::unix::fs::symlink(&dir.path().join(path), dir.path().join(&link_path)).unwrap();
    assert_eq!(
      posix_fs.stat(link_path.clone()).unwrap(),
      super::Stat::Link(Link(link_path))
    )
  }

  #[test]
  fn stat_other() {
    new_posixfs("/dev")
      .stat(PathBuf::from("null"))
      .expect_err("Want error");
  }

  #[test]
  fn stat_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    posix_fs
      .stat(PathBuf::from("no_marmosets"))
      .expect_err("Want error");
  }

  #[test]
  fn scandir_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    let path = PathBuf::from("empty_enclosure");
    std::fs::create_dir(dir.path().join(&path)).unwrap();
    assert_eq!(posix_fs.scandir(&Dir(path)).wait().unwrap(), vec![]);
  }

  #[test]
  fn scandir() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    let path = PathBuf::from("enclosure");
    std::fs::create_dir(dir.path().join(&path)).unwrap();

    let a_marmoset = path.join("a_marmoset");
    let feed = path.join("feed");
    let hammock = path.join("hammock");
    let remarkably_similar_marmoset = path.join("remarkably_similar_marmoset");
    let sneaky_marmoset = path.join("sneaky_marmoset");

    make_file(&dir.path().join(&feed), &[], 0o700);
    make_file(&dir.path().join(&a_marmoset), &[], 0o600);
    make_file(&dir.path().join(&sneaky_marmoset), &[], 0o600);
    std::os::unix::fs::symlink(
      &dir.path().join(&a_marmoset),
      dir
        .path()
        .join(&dir.path().join(&remarkably_similar_marmoset)),
    ).unwrap();
    std::fs::create_dir(dir.path().join(&hammock)).unwrap();
    make_file(
      &dir.path().join(&hammock).join("napping_marmoset"),
      &[],
      0o600,
    );

    assert_eq!(
      posix_fs.scandir(&Dir(path)).wait().unwrap(),
      vec![
        Stat::File(File {
          path: a_marmoset,
          is_executable: false,
        }),
        Stat::File(File {
          path: feed,
          is_executable: true,
        }),
        Stat::Dir(Dir(hammock)),
        Stat::Link(Link(remarkably_similar_marmoset)),
        Stat::File(File {
          path: sneaky_marmoset,
          is_executable: false,
        }),
      ]
    );
  }

  #[test]
  fn scandir_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let posix_fs = new_posixfs(&dir.path());
    posix_fs
      .scandir(&Dir(PathBuf::from("no_marmosets_here")))
      .wait()
      .expect_err("Want error");
  }

  #[test]
  fn path_stats_for_paths() {
    let dir = tempfile::TempDir::new().unwrap();
    let root_path = dir.path();

    // File tree:
    // dir
    // dir/recursive_symlink -> ../symlink -> executable_file
    // dir_symlink -> dir
    // executable_file
    // regular_file
    // symlink -> executable_file
    // symlink_to_nothing -> doesnotexist

    make_file(&root_path.join("executable_file"), &[], 0o700);
    make_file(&root_path.join("regular_file"), &[], 0o600);
    std::fs::create_dir(&root_path.join("dir")).unwrap();
    std::os::unix::fs::symlink("executable_file", &root_path.join("symlink")).unwrap();
    std::os::unix::fs::symlink(
      "../symlink",
      &root_path.join("dir").join("recursive_symlink"),
    ).unwrap();
    std::os::unix::fs::symlink("dir", &root_path.join("dir_symlink")).unwrap();
    std::os::unix::fs::symlink("doesnotexist", &root_path.join("symlink_to_nothing")).unwrap();

    let posix_fs = Arc::new(new_posixfs(&root_path));
    let path_stats = posix_fs
      .path_stats(vec![
        PathBuf::from("executable_file"),
        PathBuf::from("regular_file"),
        PathBuf::from("dir"),
        PathBuf::from("symlink"),
        PathBuf::from("dir").join("recursive_symlink"),
        PathBuf::from("dir_symlink"),
        PathBuf::from("symlink_to_nothing"),
        PathBuf::from("doesnotexist"),
      ])
      .wait()
      .unwrap();
    let v: Vec<Option<PathStat>> = vec![
      Some(PathStat::file(
        PathBuf::from("executable_file"),
        File {
          path: PathBuf::from("executable_file"),
          is_executable: true,
        },
      )),
      Some(PathStat::file(
        PathBuf::from("regular_file"),
        File {
          path: PathBuf::from("regular_file"),
          is_executable: false,
        },
      )),
      Some(PathStat::dir(
        PathBuf::from("dir"),
        Dir(PathBuf::from("dir")),
      )),
      Some(PathStat::file(
        PathBuf::from("symlink"),
        File {
          path: PathBuf::from("executable_file"),
          is_executable: true,
        },
      )),
      Some(PathStat::file(
        PathBuf::from("dir").join("recursive_symlink"),
        File {
          path: PathBuf::from("executable_file"),
          is_executable: true,
        },
      )),
      Some(PathStat::dir(
        PathBuf::from("dir_symlink"),
        Dir(PathBuf::from("dir")),
      )),
      None,
      None,
    ];
    assert_eq!(v, path_stats);
  }

  fn assert_only_file_is_executable(path: &Path, want_is_executable: bool) {
    let fs = new_posixfs(path);
    let stats = fs.scandir(&Dir(PathBuf::from("."))).wait().unwrap();
    assert_eq!(stats.len(), 1);
    match stats.get(0).unwrap() {
      &super::Stat::File(File {
        is_executable: got, ..
      }) => assert_eq!(want_is_executable, got),
      other => panic!("Expected file, got {:?}", other),
    }
  }

  fn new_posixfs<P: AsRef<Path>>(dir: P) -> PosixFS {
    PosixFS::new(
      dir.as_ref(),
      Arc::new(ResettablePool::new("test-pool-".to_string())),
      vec![],
    ).unwrap()
  }
}
