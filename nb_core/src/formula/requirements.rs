//! A formula's requirements (`depends_on macos:`, `arch:`, `xcode:` and
//! `:linux`), checked as Homebrew's `FormulaInstaller#check_requirements`
//! checks them before installing.

use serde_json::Value;

use super::types::Formula;

/// What the requirements are checked against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct System {
    /// The macOS version, `None` on Linux.
    pub macos: Option<(u32, u32)>,
    pub arm: bool,
    /// The installed Xcode.app's version, if any (`Some("")` when its
    /// version is unknown).
    pub xcode: Option<String>,
}

/// The messages of the requirements `formula` does not meet on `system`,
/// each as `name: message`. Requirements of building (`=> :build`) and
/// testing count only when `building` from source, as Homebrew prunes them
/// for a bottle. Requirements nextbrew does not know are not checked.
pub fn unsatisfied_requirements(formula: &Formula, building: bool, system: &System) -> Vec<String> {
    formula
        .requirements
        .iter()
        .filter(|req| {
            let contexts = req.get("contexts").and_then(Value::as_array);
            let tagged = |tag: &str| contexts.is_some_and(|c| c.iter().any(|t| t == tag));
            building || !(tagged("build") || tagged("test"))
        })
        .filter_map(|req| unmet(req, system))
        .map(|message| format!("{}: {message}", formula.name))
        .collect()
}

/// The error Homebrew raises once the messages are printed.
pub fn unsatisfied_error(count: usize) -> &'static str {
    if count == 1 {
        "An unsatisfied requirement failed this build."
    } else {
        "Unsatisfied requirements failed this build."
    }
}

fn unmet(req: &Value, system: &System) -> Option<String> {
    let name = req.get("name").and_then(Value::as_str)?;
    let versions: Vec<String> = match req.get("version") {
        Some(Value::String(v)) => vec![v.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    match name {
        "linux" => system
            .macos
            .is_some()
            .then(|| "Linux is required for this software.".to_string()),
        "macos" | "maximum_macos" => {
            // `depends_on maximum_macos:` only applies on macOS.
            let Some(current) = system.macos else {
                return (name == "macos").then(|| "This formula requires macOS.".to_string());
            };
            let wanted: Vec<(u32, u32)> = versions.iter().filter_map(|v| parse_macos(v)).collect();
            let satisfied = match (name, wanted.as_slice()) {
                (_, []) => true,
                ("maximum_macos", [max, ..]) => current <= *max,
                (_, [min]) if !matches!(req.get("version"), Some(Value::Array(_))) => {
                    current >= *min
                }
                (_, any) => any.contains(&current),
            };
            (!satisfied).then(|| macos_message(name, req, &wanted))
        }
        "arch" => {
            let arch = versions.first()?;
            let satisfied = match arch.as_str() {
                "arm64" | "arm" => system.arm,
                "x86_64" | "intel" => !system.arm,
                _ => true,
            };
            (!satisfied).then(|| format!("The {arch} architecture is required for this software."))
        }
        // Xcode is only a macOS requirement.
        "xcode" if system.macos.is_some() => {
            let wanted = versions.first();
            let satisfied = match (&system.xcode, wanted) {
                (None, _) => false,
                (Some(_), None) => true,
                (Some(installed), Some(wanted)) => version_at_least(installed, wanted),
            };
            (!satisfied).then(|| {
                let version = wanted.map(|v| format!(" {v}")).unwrap_or_default();
                format!(
                    "A full installation of Xcode.app{version} is required to compile\n\
                     this software. Installing just the Command Line Tools is not sufficient.\n\
                     Xcode can be installed from the App Store.\n"
                )
            })
        }
        _ => None,
    }
}

/// The wording of `MacOSRequirement#message` on macOS.
fn macos_message(name: &str, req: &Value, wanted: &[(u32, u32)]) -> String {
    let pretty = |v: &(u32, u32)| pretty_name(*v);
    if name == "maximum_macos" {
        return format!(
            "This formula either does not compile or function as expected on macOS\n\
             versions newer than {} due to an upstream incompatibility.\n",
            pretty(&wanted[0])
        );
    }
    if !matches!(req.get("version"), Some(Value::Array(_))) {
        return format!(
            "This formula does not run on macOS versions older than {}.",
            pretty(&wanted[0])
        );
    }
    match wanted {
        [only] => format!(
            "This formula does not run on macOS versions other than {}.",
            pretty(only)
        ),
        [rest @ .., last] => format!(
            "This formula does not run on macOS versions other than {} and {}.",
            rest.iter().map(pretty).collect::<Vec<_>>().join(", "),
            pretty(last)
        ),
        [] => "This formula requires macOS.".to_string(),
    }
}

/// `MacOSVersion#pretty_name`: `Big Sur` for 11.
fn pretty_name(version: (u32, u32)) -> String {
    const NAMES: &[((u32, u32), &str)] = &[
        ((27, 0), "golden_gate"),
        ((26, 0), "tahoe"),
        ((15, 0), "sequoia"),
        ((14, 0), "sonoma"),
        ((13, 0), "ventura"),
        ((12, 0), "monterey"),
        ((11, 0), "big_sur"),
        ((10, 15), "catalina"),
        ((10, 14), "mojave"),
        ((10, 13), "high_sierra"),
    ];
    match NAMES.iter().find(|(v, _)| *v == version) {
        Some((_, name)) => name
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(" "),
        None if version.1 == 0 => version.0.to_string(),
        None => format!("{}.{}", version.0, version.1),
    }
}

/// A macOS version as the API writes it: `26`, `10.15`.
fn parse_macos(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    // From Big Sur on, only the major version names a macOS release.
    Some(if major >= 11 {
        (major, 0)
    } else {
        (major, minor)
    })
}

/// Whether dotted `installed` is at least `wanted`.
fn version_at_least(installed: &str, wanted: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (installed, wanted) = (parse(installed), parse(wanted));
    for i in 0..installed.len().max(wanted.len()) {
        let (a, b) = (
            installed.get(i).copied().unwrap_or(0),
            wanted.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a > b;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn formula(requirements: Value) -> Formula {
        let mut f: Formula = serde_json::from_value(json!({
            "name": "foo",
            "versions": {"stable": "1.0"},
            "dependencies": [],
            "bottle": {},
        }))
        .unwrap();
        f.requirements = serde_json::from_value(requirements).unwrap();
        f
    }

    fn mac(major: u32, arm: bool, xcode: Option<&str>) -> System {
        System {
            macos: Some((major, 0)),
            arm,
            xcode: xcode.map(str::to_string),
        }
    }

    #[test]
    fn checks_the_macos_version() {
        let f = formula(json!([{"name": "macos", "version": "26", "contexts": []}]));
        assert!(unsatisfied_requirements(&f, false, &mac(26, true, None)).is_empty());
        assert_eq!(
            unsatisfied_requirements(&f, false, &mac(15, true, None)),
            ["foo: This formula does not run on macOS versions older than Tahoe."]
        );
        let linux = System::default();
        assert_eq!(
            unsatisfied_requirements(&f, false, &linux),
            ["foo: This formula requires macOS."]
        );

        let f = formula(json!([{"name": "maximum_macos", "version": "14", "contexts": []}]));
        assert!(unsatisfied_requirements(&f, false, &mac(14, true, None)).is_empty());
        assert!(unsatisfied_requirements(&f, false, &linux).is_empty());
        assert!(
            unsatisfied_requirements(&f, false, &mac(15, true, None))[0]
                .contains("versions newer than Sonoma")
        );

        let f = formula(json!([{"name": "macos", "version": ["13", "14"], "contexts": []}]));
        assert!(unsatisfied_requirements(&f, false, &mac(14, true, None)).is_empty());
        assert_eq!(
            unsatisfied_requirements(&f, false, &mac(15, true, None)),
            ["foo: This formula does not run on macOS versions other than Ventura and Sonoma."]
        );
    }

    #[test]
    fn checks_arch_linux_and_xcode() {
        let f = formula(json!([{"name": "arch", "version": "x86_64", "contexts": []}]));
        assert_eq!(
            unsatisfied_requirements(&f, false, &mac(26, true, None)),
            ["foo: The x86_64 architecture is required for this software."]
        );
        assert!(unsatisfied_requirements(&f, false, &mac(26, false, None)).is_empty());

        let f = formula(json!([{"name": "linux", "contexts": []}]));
        assert_eq!(
            unsatisfied_requirements(&f, false, &mac(26, true, None)),
            ["foo: Linux is required for this software."]
        );

        let f = formula(json!([{"name": "xcode", "version": "13.1", "contexts": []}]));
        assert!(
            unsatisfied_requirements(&f, false, &mac(26, true, None))[0]
                .starts_with("foo: A full installation of Xcode.app 13.1 is required")
        );
        assert!(unsatisfied_requirements(&f, false, &mac(26, true, Some("12.4"))).len() == 1);
        assert!(unsatisfied_requirements(&f, false, &mac(26, true, Some("26.0"))).is_empty());
    }

    #[test]
    fn build_requirements_count_only_when_building() {
        let f = formula(json!([
            {"name": "xcode", "version": "26.4", "contexts": ["build"]},
            {"name": "macfuse", "contexts": []},
        ]));
        let system = mac(26, true, None);
        assert!(unsatisfied_requirements(&f, false, &system).is_empty());
        assert_eq!(unsatisfied_requirements(&f, true, &system).len(), 1);
    }
}
