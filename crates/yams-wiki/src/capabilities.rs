use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CapabilityContracts {
    /// Search-hit JSON contract owned by `yams` / `memory-search`.
    ///
    /// `yams-wiki capabilities` reports this as a workspace dump; that
    /// binary does not search.
    pub search_results: u32,
    /// Canonical repository-memory layout version.
    pub repository_layout: u32,
    /// Inspect/plan/apply manifest contract version.
    pub init_manifest: u32,
    /// `check` / `compat` / `catalog` / `write` contract version.
    pub wiki_maintenance: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub ok: bool,
    pub yams_version: &'static str,
    pub contracts: CapabilityContracts,
}

pub const fn capabilities() -> Capabilities {
    Capabilities {
        ok: true,
        yams_version: env!("CARGO_PKG_VERSION"),
        contracts: CapabilityContracts {
            search_results: 1,
            repository_layout: 1,
            init_manifest: 3,
            wiki_maintenance: 2,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_pin_the_initial_public_contracts() {
        assert_eq!(
            capabilities(),
            Capabilities {
                ok: true,
                yams_version: env!("CARGO_PKG_VERSION"),
                contracts: CapabilityContracts {
                    search_results: 1,
                    repository_layout: 1,
                    init_manifest: 3,
                    wiki_maintenance: 2,
                },
            }
        );
    }
}
