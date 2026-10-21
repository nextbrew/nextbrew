//! `brew search` over the installed taps as well as Homebrew's own:
//! `Formula.full_names` and `Tap#cask_tokens` name a tap's formulae and
//! casks in full (`user/repo/name`), and the query matches those full
//! names. A tap's formula is listed whether or not it is trusted (brew
//! lists the names it could not load); a cask only once it is trusted.

use nb_core::Error;

use super::Installer;
use crate::tap::TapName;
use crate::tap::trust::TrustKind;

type Matcher = Box<dyn Fn(&str) -> bool>;

/// `Search.search`: `/regex/`, or else the query's letters, digits, `@`
/// and `+` within the name's.
fn matcher(query: &str) -> Result<Matcher, Error> {
    if query.len() > 1
        && let Some(pattern) = query.strip_prefix('/').and_then(|q| q.strip_suffix('/'))
    {
        let regex = regex::Regex::new(pattern).map_err(|_| Error::InvalidArgument {
            message: format!("{query} is not a valid regex."),
        })?;
        return Ok(Box::new(move |name| regex.is_match(name)));
    }
    fn simplify(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '@' || *c == '+')
            .collect::<String>()
            .to_ascii_lowercase()
    }
    let query = simplify(query);
    Ok(Box::new(move |name| simplify(name).contains(&query)))
}

fn is_regex(query: &str) -> bool {
    query.len() > 1 && query.starts_with('/') && query.ends_with('/')
}

/// `HOMEBREW_TAP_FORMULA_REGEX` / `HOMEBREW_TAP_CASK_REGEX`: a query naming
/// one tap's formula or cask in full.
fn tap_name(query: &str) -> Option<(TapName, String)> {
    if is_regex(query) {
        return None;
    }
    TapName::split_full_name(query).filter(|(tap, _)| !tap.is_core())
}

/// The sort key of `search_formulae`: the name before `@`, then the
/// version after it.
fn formula_key(name: &str) -> (String, Vec<u64>) {
    let (base, version) = name.split_once('@').unwrap_or((name, ""));
    let parts = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    (base.to_string(), parts)
}

impl Installer {
    /// The installed third-party taps.
    fn search_taps(&self) -> Vec<TapName> {
        self.tap_source()
            .map(|source| {
                source
                    .taps
                    .installed()
                    .into_iter()
                    .filter(|tap| !tap.is_core())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `Search.search_formulae`.
    pub async fn search_formulas(&self, query: &str) -> Result<Vec<String>, Error> {
        if let Some((tap, name)) = tap_name(query) {
            // `Formulary.factory(...).name`.
            let source = self.tap_source()?;
            return Ok(source
                .taps
                .find_formula(&tap, &name)
                .map(|(name, _)| vec![name])
                .unwrap_or_default());
        }
        let mut results = self.api_client.search_formulas(query).await?;
        let taps = self.search_taps();
        if taps.is_empty() {
            return Ok(results);
        }
        let source = self.tap_source()?;
        let matches = matcher(query)?;
        let mut found = Vec::new();
        for tap in &taps {
            let formulae = source.taps.formula_files(tap);
            for name in formulae.keys() {
                found.push(format!("{tap}/{name}"));
            }
            // An alias, unless its formula is listed too.
            for (alias, formula) in source.taps.aliases(tap) {
                let formula = format!("{tap}/{formula}");
                if !matches(&formula) {
                    found.push(format!("{tap}/{alias}"));
                }
            }
        }
        found.retain(|name| matches(name));
        found.sort_by_cached_key(|name| formula_key(name));
        // Into the sorted list Homebrew's own came in.
        for name in found {
            if results.contains(&name) {
                continue;
            }
            let key = formula_key(&name);
            let at = results
                .iter()
                .position(|other| formula_key(other) > key)
                .unwrap_or(results.len());
            results.insert(at, name);
        }
        Ok(results)
    }

    /// `Search.search_casks`.
    pub async fn search_casks(&self, query: &str) -> Result<Vec<String>, Error> {
        if let Some((tap, token)) = tap_name(query) {
            let source = self.tap_source()?;
            let remote = source.taps.remote(&tap);
            return Ok(source
                .taps
                .find_cask(&tap, &token)
                .filter(|(token, _)| {
                    source
                        .trust
                        .trusted(TrustKind::Cask, &tap, token, remote.as_deref())
                })
                .map(|(token, _)| vec![token])
                .unwrap_or_default());
        }
        let taps = self.search_taps();
        if taps.is_empty() {
            return self.api_client.search_casks(query).await;
        }
        let source = self.tap_source()?;
        let mut tokens: Vec<String> = self
            .api_client
            .all_casks()
            .await?
            .iter()
            .filter_map(|cask| cask["token"].as_str().map(str::to_string))
            .collect();
        for tap in &taps {
            for token in source.taps.cask_files(tap).keys() {
                tokens.push(format!("{tap}/{token}"));
            }
        }
        let matches = matcher(query)?;
        let mut results: Vec<String> = tokens.iter().filter(|t| matches(t)).cloned().collect();
        if !is_regex(query) {
            results.extend(crate::network::suggest::did_you_mean(query, &tokens));
        }
        results.sort();
        results.dedup();
        // A tap's cask that is not trusted is left out.
        results.retain(|name| match TapName::split_full_name(name) {
            Some((tap, token)) => {
                let remote = source.taps.remote(&tap);
                source
                    .trust
                    .trusted(TrustKind::Cask, &tap, &token, remote.as_deref())
            }
            None => true,
        });
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_full_names_as_homebrew_does() {
        let matches = matcher("tap-terra").unwrap();
        assert!(matches("hashicorp/tap/terraform"));
        assert!(!matches("terraform"));
        let regex = matcher("/^hashicorp\\//").unwrap();
        assert!(regex("hashicorp/tap/vault"));
        assert!(!regex("vault"));
    }

    #[test]
    fn a_query_in_full_names_one_taps_package() {
        assert_eq!(
            tap_name("hashicorp/tap/terraform"),
            Some((TapName::parse("hashicorp/tap").unwrap(), "terraform".into()))
        );
        assert_eq!(tap_name("homebrew/core/jq"), None);
        assert_eq!(tap_name("/a/b/c/"), None);
        assert_eq!(tap_name("jq"), None);
    }

    #[test]
    fn sorts_by_name_then_version() {
        let mut names = vec!["a/b/node", "node@9", "node@18", "node", "nodenv"];
        names.sort_by_cached_key(|name| formula_key(name));
        assert_eq!(
            names,
            vec!["a/b/node", "node", "node@9", "node@18", "nodenv"]
        );
    }
}
