//! Query and run Paru as the target’s non-root build account.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use dek_core::{
    Action, ActionKind, AurBuildDependencies, AurBuildStamp, PackageId, PackageSource, Precondition,
};
use dek_target::{
    backend::{ActionContext, ActionReadiness, BackendError, PlannedAction},
    terminal::TerminalCommand,
};
use rustix::process::geteuid;

use crate::accounts::{UserAccount, user_account, user_account_by_uid};

use super::{PacmanBackend, package_version, valid_package_name, valid_version};

const BUILD_USER_FILE: &str = "/etc/dek/aur-build-user";
const BUILD_PATH: &str = "/usr/local/bin:/usr/bin";
const TOOLS: &[&str] = &["git", "makepkg", "make", "gcc", "fakeroot", "gpg", "vercmp"];

pub(super) struct BuildEnvironment {
    account: UserAccount,
    helper: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub version: String,
    pub dependencies: AurBuildDependencies,
}

impl BuildEnvironment {
    pub fn discover() -> Result<Self, BackendError> {
        let helper =
            executable("paru").ok_or_else(|| BackendError("目标机缺少 AUR helper paru".into()))?;
        let missing: Vec<_> = TOOLS
            .iter()
            .filter(|name| executable(name).is_none())
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(BackendError(format!(
                "目标机缺少 AUR 构建工具：{}",
                missing.join(", ")
            )));
        }

        let uid = geteuid().as_raw();
        let account = if uid == 0 {
            let name = configured_build_user()?;
            user_account(&name)?
                .ok_or_else(|| BackendError(format!("目标机没有配置的 AUR 构建用户 {name:?}")))?
        } else {
            user_account_by_uid(uid)?
                .ok_or_else(|| BackendError(format!("无法定位 UID {uid} 的 AUR 构建用户")))?
        };
        if account.uid == 0 || !account.home.is_absolute() || !account.home.is_dir() {
            return Err(BackendError(format!(
                "AUR 构建需要非 root 用户及有效的主目录，当前配置为 {:?}",
                account.name
            )));
        }
        Ok(Self { account, helper })
    }

    pub fn invocation(&self, args: impl IntoIterator<Item = OsString>) -> TerminalCommand {
        let mut arguments = Vec::new();
        let program = if geteuid().as_raw() == self.account.uid {
            self.helper.as_os_str()
        } else {
            let groups = self
                .account
                .groups
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            arguments.extend([
                "--reuid".into(),
                self.account.uid.to_string().into(),
                "--regid".into(),
                self.account.gid.to_string().into(),
                "--groups".into(),
                groups.into(),
                "--".into(),
                self.helper.as_os_str().into(),
            ]);
            "/usr/bin/setpriv".as_ref()
        };
        arguments.extend(args);
        TerminalCommand::new(program, arguments).in_environment(
            self.account.home.clone(),
            [
                ("PATH".into(), BUILD_PATH.into()),
                ("LC_ALL".into(), "C".into()),
                ("HOME".into(), self.account.home.as_os_str().into()),
                ("USER".into(), self.account.name.as_str().into()),
                ("LOGNAME".into(), self.account.name.as_str().into()),
            ],
        )
    }

    pub fn candidates(
        &self,
        names: &BTreeSet<&str>,
    ) -> Result<BTreeMap<String, Candidate>, BackendError> {
        let args = ["-Si", "--aur", "--color", "never", "--"]
            .into_iter()
            .chain(names.iter().copied())
            .map(OsString::from);
        let output = self
            .invocation(args)
            .command()
            .output()
            .map_err(|err| BackendError(format!("无法查询 paru 的受管 AUR 候选：{err}")))?;
        if !output.status.success() {
            return Err(BackendError(format!(
                "paru 查询 AUR 候选失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|err| BackendError(format!("paru 的 AUR 候选结果不是 UTF-8：{err}")))?;
        let candidates = parse_candidates(text)?;
        if candidates
            .keys()
            .map(String::as_str)
            .ne(names.iter().copied())
        {
            return Err(BackendError(format!(
                "paru 未准确返回受管 AUR 候选：请求 {}，返回 {}",
                names.iter().copied().collect::<Vec<_>>().join(", "),
                candidates
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(candidates)
    }
}

fn compare_versions(installed: &str, candidate: &str) -> Result<i32, BackendError> {
    let output = Command::new("/usr/bin/vercmp")
        .args([installed, candidate])
        .env("LC_ALL", "C")
        .output()
        .map_err(|err| BackendError(format!("无法比较 AUR 包版本：{err}")))?;
    let version_order = std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok());
    match (output.status.success(), version_order) {
        (true, Some(order)) => Ok(order),
        _ => Err(BackendError(format!(
            "无法比较 AUR 包版本 {installed:?} 和 {candidate:?}"
        ))),
    }
}

pub(super) fn plan_build(
    package: &PackageId,
    version: Option<&str>,
    candidate: Candidate,
    rebuild: bool,
    reviewed: Option<&[Action]>,
    preconditions: Vec<Precondition>,
) -> Result<Option<PlannedAction>, BackendError> {
    let prior = reviewed.and_then(|actions| {
        actions.iter().find_map(|action| {
            let build = ReviewedBuild::from_kind(&action.kind).ok()?;
            (build.package == package).then_some(build)
        })
    });
    let name = &package.name;
    let mut rebuild_from = None;
    let reason = if let Some(version) = version {
        let order = compare_versions(version, &candidate.version)?;
        if rebuild
            && reviewed.is_none_or(|_| prior.as_ref().is_some_and(|p| p.rebuild_from.is_some()))
        {
            if order > 0 {
                return Err(BackendError(format!(
                    "受管 AUR 包 {name} 的已安装版本 {version} 高于候选 {}，无法按候选重建",
                    candidate.version
                )));
            }
            let stamp = build_stamp(package, version)?;
            if candidate.version == version
                && prior.is_some_and(|build| build.satisfies(Some(version), Some(&stamp)))
            {
                return Ok(None);
            }
            rebuild_from = Some(stamp);
            format!("已请求重新构建受管 AUR 包 {name} {version}")
        } else if order < 0 {
            format!(
                "受管 AUR 包 {name} 已安装 {version}，候选为 {}",
                candidate.version
            )
        } else {
            return Ok(None);
        }
    } else {
        format!("受管 AUR 包 {name} 尚未安装")
    };
    Ok(Some(PlannedAction {
        kind: match rebuild_from {
            Some(rebuild_from) => ActionKind::RebuildAurPackage {
                package: package.clone(),
                candidate_version: candidate.version,
                rebuild_from,
                dependencies: candidate.dependencies,
            },
            None => ActionKind::BuildAurPackage {
                package: package.clone(),
                candidate_version: Some(candidate.version),
                dependencies: candidate.dependencies,
            },
        },
        reason,
        preconditions,
        content_changed: false,
    }))
}

pub(super) struct ReviewedBuild<'a> {
    package: &'a PackageId,
    candidate_version: &'a str,
    dependencies: &'a AurBuildDependencies,
    rebuild_from: Option<&'a AurBuildStamp>,
}

impl<'a> ReviewedBuild<'a> {
    fn from_kind(kind: &'a ActionKind) -> Result<Self, BackendError> {
        let (package, candidate_version, dependencies, rebuild_from) = match kind {
            ActionKind::BuildAurPackage {
                package,
                candidate_version: Some(version),
                dependencies,
            } => (package, version, dependencies, None),
            ActionKind::RebuildAurPackage {
                package,
                candidate_version,
                dependencies,
                rebuild_from,
            } => (package, candidate_version, dependencies, Some(rebuild_from)),
            _ => return Err(BackendError("AUR 构建缺少已审阅的候选版本".into())),
        };
        if package.source != PackageSource::Aur
            || !valid_package_name(&package.name)
            || !valid_candidate_version(candidate_version)
        {
            return Err(BackendError("AUR 构建包名、来源或候选版本无效".into()));
        }
        Ok(Self {
            package,
            candidate_version,
            dependencies,
            rebuild_from,
        })
    }

    fn satisfies(&self, version: Option<&str>, stamp: Option<&AurBuildStamp>) -> bool {
        version == Some(self.candidate_version)
            && self
                .rebuild_from
                .is_none_or(|before| stamp.is_some_and(|after| after != before))
    }
}

pub(super) fn reviewed_build(action: &Action) -> Result<ReviewedBuild<'_>, BackendError> {
    let build = ReviewedBuild::from_kind(&action.kind)?;
    let previous = package_version(build.package, &action.preconditions)?;
    if build.rebuild_from.is_some() && previous.is_none() {
        return Err(BackendError("AUR 重建缺少已安装版本的前置条件".into()));
    }
    Ok(build)
}

impl PacmanBackend {
    pub(super) fn inspect_aur(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let build = reviewed_build(action)?;
        let expected = package_version(build.package, &action.preconditions)?;
        let (installed, stamp) = self.observe_aur_installation(&build)?;
        Ok(if build.satisfies(installed.as_deref(), stamp.as_ref()) {
            ActionReadiness::Satisfied
        } else if installed.as_deref() == expected && stamp.as_ref() == build.rebuild_from {
            ActionReadiness::Ready
        } else {
            ActionReadiness::Conflict(format!(
                "AUR 软件包 {} 的已安装版本或构建记录已变，请重新规划",
                build.package.name
            ))
        })
    }

    pub(super) fn execute_aur(&self, context: &ActionContext<'_>) -> Result<bool, BackendError> {
        let build = reviewed_build(context.action)?;
        match self.inspect_aur(context.action)? {
            ActionReadiness::Ready => {}
            ActionReadiness::Satisfied => return Ok(false),
            ActionReadiness::Conflict(reason) => return Err(BackendError(reason)),
        }
        let name = build.package.name.as_str();
        let environment = BuildEnvironment::discover()?;
        let candidates = environment.candidates(&BTreeSet::from([name]))?;
        let candidate = &candidates[name];
        if candidate.version != build.candidate_version
            || candidate.dependencies != *build.dependencies
        {
            return Err(BackendError(format!(
                "AUR 软件包 {name} 的候选或依赖已变，请重新规划"
            )));
        }
        let target = format!("{name}={}", build.candidate_version);
        let args = ["-S", "--aur", "--review"]
            .into_iter()
            .chain(build.rebuild_from.map(|_| "--rebuild"))
            .chain(["--", &target])
            .map(OsString::from);
        context
            .run_interactive(&environment.invocation(args))
            .map_err(|error| BackendError(format!("构建受管 AUR 包 {name} 失败：{error}")))?;
        Ok(true)
    }

    pub(super) fn verify_aur(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let build = ReviewedBuild::from_kind(action)?;
        let (installed, stamp) = self.observe_aur_installation(&build)?;
        Ok(build.satisfies(installed.as_deref(), stamp.as_ref()))
    }

    fn observe_aur_installation(
        &self,
        build: &ReviewedBuild<'_>,
    ) -> Result<(Option<String>, Option<AurBuildStamp>), BackendError> {
        let installed = self.installed(build.package)?;
        let Some(version) = installed.as_deref() else {
            return Ok((None, None));
        };
        // Querying names and versions alone can find incomplete entries left
        // by an interrupted transaction. Read the description and file list.
        self.inspect_installed_records([build.package.name.as_str()])?;
        let stamp = build
            .rebuild_from
            .map(|_| build_stamp(build.package, version))
            .transpose()?;
        Ok((installed, stamp))
    }
}

fn valid_candidate_version(version: &str) -> bool {
    valid_version(version) && !version.contains(['/', '=', '<', '>'])
}

fn build_stamp(package: &PackageId, version: &str) -> Result<AurBuildStamp, BackendError> {
    if !valid_package_name(&package.name) || !valid_version(version) || version.contains('/') {
        return Err(BackendError("无法读取无效包名或版本的 AUR 构建记录".into()));
    }
    let output = Command::new("/usr/bin/pacman-conf")
        .arg("DBPath")
        .env("LC_ALL", "C")
        .output()
        .map_err(|err| BackendError(format!("无法读取 pacman DBPath：{err}")))?;
    let path = String::from_utf8_lossy(&output.stdout);
    let db = Path::new(path.trim_end_matches('\n'));
    if !output.status.success() || !db.is_absolute() {
        return Err(BackendError("无法读取 pacman 本地数据库路径".into()));
    }
    // pacman replaces the entry on reinstall. A change of install reason only
    // updates the desc file and leaves the entry's generation intact.
    let entry = db.join("local").join(format!("{}-{version}", package.name));
    let metadata = fs::metadata(&entry)
        .map_err(|error| BackendError(format!("读取 AUR 构建记录 {}：{error}", entry.display())))?;
    Ok(AurBuildStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_at: metadata.ctime(),
        changed_at_nsec: metadata.ctime_nsec(),
    })
}

fn executable(name: &str) -> Option<PathBuf> {
    BUILD_PATH
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
        })
}

fn configured_build_user() -> Result<String, BackendError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(BUILD_USER_FILE)
        .map_err(|err| {
            BackendError(format!(
                "目标机缺少可用的非 root AUR 构建用户，请检查 {BUILD_USER_FILE}：{err}"
            ))
        })?;
    let metadata = file
        .metadata()
        .map_err(|err| BackendError(format!("读取 {BUILD_USER_FILE}：{err}")))?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(BackendError(format!(
            "AUR 构建用户配置 {BUILD_USER_FILE} 需要由 root 持有且仅允许 root 写入"
        )));
    }
    let mut text = String::new();
    file.take(1025)
        .read_to_string(&mut text)
        .map_err(|err| BackendError(format!("读取 {BUILD_USER_FILE}：{err}")))?;
    let name = text.trim();
    if name.is_empty() || text.len() > 1024 || name.contains(char::is_whitespace) {
        return Err(BackendError(format!(
            "{BUILD_USER_FILE} 应包含一个非 root AUR 构建用户名"
        )));
    }
    Ok(name.into())
}

fn parse_candidates(text: &str) -> Result<BTreeMap<String, Candidate>, BackendError> {
    let mut candidates = BTreeMap::new();
    for record in text
        .split("\n\n")
        .filter(|record| !record.trim().is_empty())
    {
        let mut fields = BTreeMap::<&str, String>::new();
        let mut last = None;
        for line in record.lines() {
            if line.starts_with(char::is_whitespace) {
                if let Some(key) = last {
                    let field = fields
                        .get_mut(key)
                        .expect("last output field is registered");
                    field.push(' ');
                    field.push_str(line.trim());
                }
            } else if let Some((key, value)) = line.split_once(':') {
                let key = key.trim_end();
                if fields.insert(key, value.trim().into()).is_some() {
                    return Err(BackendError(format!("paru 返回重复字段 {key:?}")));
                }
                last = Some(key);
            }
        }
        let invalid = || {
            BackendError(format!(
                "paru 返回无效的 AUR 候选记录：{}",
                record.lines().take(3).collect::<Vec<_>>().join("；")
            ))
        };
        let Some("aur") = fields.get("Repository").map(String::as_str) else {
            return Err(invalid());
        };
        let name = fields
            .get("Name")
            .filter(|name| valid_package_name(name))
            .ok_or_else(invalid)?;
        let version = fields
            .get("Version")
            .filter(|version| valid_candidate_version(version))
            .ok_or_else(invalid)?;
        let dependencies = |key| {
            let mut names = fields
                .get(key)
                .map(|list| {
                    list.split_ascii_whitespace()
                        .filter(|name| *name != "None")
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            names.sort_unstable();
            names.dedup();
            names
        };
        let candidate = Candidate {
            version: version.clone(),
            dependencies: AurBuildDependencies {
                runtime: dependencies("Depends On"),
                build: dependencies("Make Deps"),
                check: dependencies("Check Deps"),
            },
        };
        if candidates.insert(name.clone(), candidate).is_some() {
            return Err(BackendError(format!("paru 返回重复的 AUR 候选 {name}")));
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_candidates_and_wrapped_dependency_fields() {
        let text = "\
Repository      : aur
Name            : first
Version         : 2:1.2-3
Description     : A fixture
Depends On      : glibc  libfoo>=2
                  libbar
Make Deps       : gcc  make
Check Deps      : None

Repository      : aur
Name            : second
Version         : 1-1
Depends On      : None
Make Deps       : None
Check Deps      : bash
";
        let found = parse_candidates(text).unwrap();
        assert_eq!(found["first"].version, "2:1.2-3");
        assert_eq!(
            found["first"].dependencies.runtime,
            ["glibc", "libbar", "libfoo>=2"]
        );
        assert_eq!(found["first"].dependencies.build, ["gcc", "make"]);
        assert!(found["first"].dependencies.check.is_empty());
        assert_eq!(found["second"].dependencies.check, ["bash"]);
        assert!(parse_candidates(&format!("{text}\n{text}")).is_err());
        assert!(parse_candidates("Repository : core\nName : first\nVersion : 1-1").is_err());
        assert!(parse_candidates("Repository : aur\nName : first").is_err());
    }
}
