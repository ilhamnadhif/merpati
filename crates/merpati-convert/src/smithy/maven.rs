//! Models of the Maven dependencies of a `smithy-build.json`
//! (`maven.dependencies`), read from jars that are already in the local
//! Maven repository. Nothing is downloaded: a jar that is not there is
//! reported by its coordinates and its shapes stay unknown.
//!
//! A Smithy model jar lists its model files in `META-INF/smithy/manifest`
//! (one path per line, relative to `META-INF/smithy/`, `#` comments), as
//! Smithy's own model discovery reads it. The dependencies of a dependency
//! come from the `.pom` next to its jar: compile and runtime scope, not
//! optional, versions written out or as `${property}` (parent POMs and
//! managed versions are not followed). Like Maven, the version nearest to
//! the `smithy-build.json` wins for each `group:artifact`.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::SourceFile;

/// Larger model files are refused (AWS's biggest JSON models are ~10 MB).
const MAX_MODEL_BYTES: u64 = 64 << 20;
/// Dependencies followed at most (direct and transitive).
const MAX_ARTIFACTS: usize = 500;

/// A `group:artifact:version` Maven coordinate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MavenCoordinate {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
}

impl MavenCoordinate {
    /// Parses `group:artifact:version` (the only form `smithy-build.json`
    /// accepts).
    pub fn parse(s: &str) -> Option<MavenCoordinate> {
        let mut parts = s.trim().split(':');
        let (g, a, v) = (parts.next()?, parts.next()?, parts.next()?);
        let valid = |p: &str| !p.is_empty() && !p.contains(['/', '\\']) && p != "." && p != "..";
        (parts.next().is_none() && valid(g) && valid(a) && valid(v)).then(|| MavenCoordinate {
            group_id: g.to_string(),
            artifact_id: a.to_string(),
            version: v.to_string(),
        })
    }

    fn dir(&self, repository: &Path) -> PathBuf {
        let mut dir = repository.to_path_buf();
        dir.extend(self.group_id.split('.'));
        dir.push(&self.artifact_id);
        dir.push(&self.version);
        dir
    }

    /// `<repository>/<group as path>/<artifact>/<version>/<artifact>-<version>.jar`.
    pub fn jar_path(&self, repository: &Path) -> PathBuf {
        self.dir(repository).join(format!("{}-{}.jar", self.artifact_id, self.version))
    }

    fn pom_path(&self, repository: &Path) -> PathBuf {
        self.dir(repository).join(format!("{}-{}.pom", self.artifact_id, self.version))
    }

    /// A version range or `LATEST`/`RELEASE`, which needs repository metadata.
    fn is_dynamic(&self) -> bool {
        self.version.starts_with(['[', '(']) || self.version.ends_with('+') || {
            let v = self.version.as_str();
            v == "LATEST" || v == "RELEASE"
        }
    }
}

impl fmt::Display for MavenCoordinate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.group_id, self.artifact_id, self.version)
    }
}

/// The `maven.dependencies` of a `smithy-build.json`, as written, in order.
pub fn build_config_maven_dependencies(json: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("invalid smithy-build.json: {e}"))?;
    match v.pointer("/maven/dependencies") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|d| d.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| "smithy-build.json: `maven.dependencies` must list strings".into()),
        Some(_) => Err("smithy-build.json: `maven.dependencies` must be an array".into()),
    }
}

/// The local Maven repository: `localRepository` of `~/.m2/settings.xml`,
/// otherwise `~/.m2/repository`. `None` without a home directory.
pub fn default_maven_repository() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)?;
    let configured = std::fs::read_to_string(home.join(".m2").join("settings.xml"))
        .ok()
        .and_then(|text| crate::xml::parse(&text).ok())
        .and_then(|root| root.child("localRepository").map(|e| e.text.trim().to_string()))
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(p.replace("${user.home}", &home.to_string_lossy())));
    Some(configured.unwrap_or_else(|| home.join(".m2").join("repository")))
}

/// Model files read from the jars of `maven.dependencies`, see
/// [`resolve_maven_dependencies`].
#[derive(Clone, Debug, Default)]
pub struct MavenModels {
    /// `(name, text)` of each model file; the name is
    /// `<artifact>-<version>.jar!/META-INF/smithy/<path>`.
    pub files: Vec<(String, String)>,
    /// Missing or unreadable jars (named by their coordinates), invalid
    /// coordinates, version ranges.
    pub warnings: Vec<String>,
}

impl MavenModels {
    /// The files parsed, to assemble after the project's own files (the
    /// first file's namespace is the model's).
    pub fn sources(&self) -> Vec<SourceFile> {
        self.files.iter().map(|(name, text)| SourceFile::parse(name.as_str(), text)).collect()
    }
}

/// Reads the models of the `maven.dependencies` of a `smithy-build.json`
/// (and of their dependencies) from the local Maven repository
/// (`repository`, or [`default_maven_repository`]). Fails only on an invalid
/// `smithy-build.json`; anything not found is a warning.
pub fn resolve_maven_dependencies(build_json: &str, repository: Option<&Path>) -> Result<MavenModels, String> {
    let deps = build_config_maven_dependencies(build_json)?;
    let mut out = MavenModels::default();
    if deps.is_empty() {
        return Ok(out);
    }
    let Some(repo) = repository.map(Path::to_path_buf).or_else(default_maven_repository) else {
        out.warnings.push("Maven dependencies were not read: no home directory for ~/.m2/repository".into());
        return Ok(out);
    };
    let mut queue: VecDeque<(MavenCoordinate, Option<String>)> = VecDeque::new();
    for d in &deps {
        match MavenCoordinate::parse(d) {
            Some(c) => queue.push_back((c, None)),
            None => out.warnings.push(format!("invalid Maven dependency `{d}` (expected group:artifact:version)")),
        }
    }
    let mut seen: HashSet<(String, String)> = HashSet::new();
    while let Some((c, needed_by)) = queue.pop_front() {
        if !seen.insert((c.group_id.clone(), c.artifact_id.clone())) {
            continue;
        }
        if seen.len() > MAX_ARTIFACTS {
            out.warnings.push(format!("more than {MAX_ARTIFACTS} Maven dependencies; the rest was not read"));
            break;
        }
        let via = needed_by.map(|p| format!(" (needed by {p})")).unwrap_or_default();
        if c.is_dynamic() {
            out.warnings
                .push(format!("Maven dependency {c}{via}: version ranges are not resolved; use a fixed version"));
            continue;
        }
        let jar = c.jar_path(&repo);
        let bytes = match std::fs::read(&jar) {
            Ok(b) => b,
            Err(_) => {
                out.warnings.push(format!(
                    "Maven dependency {c}{via} is not in the local repository ({}); its shapes are unknown. Merpati \
                     does not download dependencies: fetch them with Maven, Gradle or the Smithy CLI",
                    jar.display()
                ));
                continue;
            }
        };
        match jar_model_files(&bytes) {
            Ok(files) => {
                let jar_name = format!("{}-{}.jar", c.artifact_id, c.version);
                out.files.extend(
                    files.into_iter().map(|(path, text)| (format!("{jar_name}!/META-INF/smithy/{path}"), text)),
                );
            }
            Err(e) => out.warnings.push(format!("Maven dependency {c}{via}: cannot read {}: {e}", jar.display())),
        }
        if let Ok(pom) = std::fs::read_to_string(c.pom_path(&repo)) {
            queue.extend(pom_dependencies(&pom).into_iter().map(|d| (d, Some(c.to_string()))));
        }
    }
    Ok(out)
}

/// The compile/runtime, non-optional jar dependencies of a POM whose
/// coordinates are written out or use `${...}` properties of the POM itself.
fn pom_dependencies(pom: &str) -> Vec<MavenCoordinate> {
    let Ok(root) = crate::xml::parse(pom) else { return Vec::new() };
    let text = |parent: Option<&crate::xml::Element>, name: &str| {
        parent.and_then(|p| p.child(name)).map(|e| e.text.trim().to_string()).filter(|t| !t.is_empty())
    };
    let parent = root.child("parent");
    let version = text(Some(&root), "version").or_else(|| text(parent, "version")).unwrap_or_default();
    let group = text(Some(&root), "groupId").or_else(|| text(parent, "groupId")).unwrap_or_default();
    let props = root.child("properties");
    let interpolate = |s: &str| -> Option<String> {
        let mut out = String::new();
        let mut rest = s;
        while let Some(start) = rest.find("${") {
            let end = rest[start..].find('}')? + start;
            let name = &rest[start + 2..end];
            let value = match name {
                "project.version" | "pom.version" | "version" => version.clone(),
                "project.groupId" | "pom.groupId" => group.clone(),
                _ => text(props, name)?,
            };
            out.push_str(&rest[..start]);
            out.push_str(&value);
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        (!out.is_empty() && !out.contains("${")).then_some(out)
    };
    let mut out = Vec::new();
    for d in root.child("dependencies").into_iter().flat_map(|ds| ds.children_named("dependency")) {
        let scope = text(Some(d), "scope").unwrap_or_else(|| "compile".into());
        let optional = text(Some(d), "optional").is_some_and(|o| o == "true");
        let jar = text(Some(d), "type").is_none_or(|t| t == "jar") && text(Some(d), "classifier").is_none();
        if optional || !jar || !matches!(scope.as_str(), "compile" | "runtime") {
            continue;
        }
        let field = |name: &str| text(Some(d), name).and_then(|v| interpolate(&v));
        if let (Some(g), Some(a), Some(v)) = (field("groupId"), field("artifactId"), field("version"))
            && let Some(c) = MavenCoordinate::parse(&format!("{g}:{a}:{v}"))
        {
            out.push(c);
        }
    }
    out
}

/// The model files of a Smithy jar: the `.smithy` and `.json` files listed
/// in `META-INF/smithy/manifest`, as `(path relative to META-INF/smithy/,
/// text)`. A jar without manifest holds no models.
pub fn jar_model_files(jar: &[u8]) -> Result<Vec<(String, String)>, String> {
    let entries = zip_entries(jar)?;
    let read = |name: &str| -> Result<Option<String>, String> {
        let Some(e) = entries.iter().find(|e| e.name == name) else { return Ok(None) };
        let bytes = zip_read(jar, e)?;
        String::from_utf8(bytes).map(Some).map_err(|_| format!("{name} is not UTF-8"))
    };
    let Some(manifest) = read("META-INF/smithy/manifest")? else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for line in manifest.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        // The rules of Smithy's manifest reader.
        if line.starts_with('/') || line.ends_with('/') || line.contains("..") || line.contains("//") {
            return Err(format!("invalid model path `{line}` in META-INF/smithy/manifest"));
        }
        if line.contains(['\\', ':']) {
            return Err(format!("invalid model path `{line}` in META-INF/smithy/manifest"));
        }
        if !(line.ends_with(".smithy") || line.ends_with(".json")) {
            continue;
        }
        let text = read(&format!("META-INF/smithy/{line}"))?
            .ok_or_else(|| format!("META-INF/smithy/manifest lists `{line}`, which is not in the jar"))?;
        out.push((line.to_string(), text));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Minimal zip reading (stored and deflated entries, no ZIP64)
// ---------------------------------------------------------------------------

struct ZipEntry {
    name: String,
    flags: u16,
    method: u16,
    crc: u32,
    compressed_size: usize,
    size: u64,
    local_offset: usize,
}

fn u16_at(b: &[u8], i: usize) -> Result<u16, String> {
    b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| "truncated zip".to_string())
}

fn u32_at(b: &[u8], i: usize) -> Result<u32, String> {
    b.get(i..i + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| "truncated zip".to_string())
}

/// The central directory, found through the end-of-central-directory record
/// (the last one within the final 64 KiB, which may hold a comment).
fn zip_entries(data: &[u8]) -> Result<Vec<ZipEntry>, String> {
    const EOCD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    let last = data.len().checked_sub(22).ok_or("not a zip file")?;
    let eocd = (last.saturating_sub(0xFFFF)..=last).rev().find(|&i| data[i..i + 4] == EOCD).ok_or("not a zip file")?;
    let count = u16_at(data, eocd + 10)?;
    let offset = u32_at(data, eocd + 16)?;
    if count == 0xFFFF || offset == 0xFFFF_FFFF {
        return Err("ZIP64 archives are not supported".into());
    }
    let mut pos = offset as usize;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if u32_at(data, pos)? != 0x0201_4b50 {
            return Err("corrupt zip central directory".into());
        }
        let name_len = u16_at(data, pos + 28)? as usize;
        let extra_len = u16_at(data, pos + 30)? as usize;
        let comment_len = u16_at(data, pos + 32)? as usize;
        let name = data.get(pos + 46..pos + 46 + name_len).ok_or("truncated zip")?;
        out.push(ZipEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            flags: u16_at(data, pos + 8)?,
            method: u16_at(data, pos + 10)?,
            crc: u32_at(data, pos + 16)?,
            compressed_size: u32_at(data, pos + 20)? as usize,
            size: u64::from(u32_at(data, pos + 24)?),
            local_offset: u32_at(data, pos + 42)? as usize,
        });
        pos += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

fn zip_read(data: &[u8], e: &ZipEntry) -> Result<Vec<u8>, String> {
    if e.flags & 1 != 0 {
        return Err(format!("{} is encrypted", e.name));
    }
    if e.size > MAX_MODEL_BYTES {
        return Err(format!("{} is larger than {} MB", e.name, MAX_MODEL_BYTES >> 20));
    }
    let at = e.local_offset;
    if u32_at(data, at)? != 0x0403_4b50 {
        return Err(format!("corrupt zip entry {}", e.name));
    }
    let start = at + 30 + u16_at(data, at + 26)? as usize + u16_at(data, at + 28)? as usize;
    let raw = data.get(start..start + e.compressed_size).ok_or("truncated zip")?;
    let bytes = match e.method {
        0 => raw.to_vec(),
        8 => {
            let mut out = Vec::with_capacity(e.size as usize);
            flate2::read::DeflateDecoder::new(raw)
                .take(e.size + 1)
                .read_to_end(&mut out)
                .map_err(|err| format!("{}: {err}", e.name))?;
            out
        }
        m => return Err(format!("{} uses unsupported zip compression method {m}", e.name)),
    };
    let mut crc = flate2::Crc::new();
    crc.update(&bytes);
    if bytes.len() as u64 != e.size || crc.sum() != e.crc {
        return Err(format!("corrupt zip entry {} (size or CRC mismatch)", e.name));
    }
    Ok(bytes)
}
