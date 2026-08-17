use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutClass {
    Absent,
    Minimal,
    Full,
    Partial,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InitMode {
    Minimal,
    Full,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    CreateDirectory,
    CreateFile,
    ReplaceFile,
    RemoveFile,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodePrestate {
    pub path: String,
    pub kind: NodeKind,
    pub mode: Option<u32>,
    pub sha256: Option<String>,
    pub entries_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitConflict {
    pub path: String,
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPageRequest {
    pub title: String,
    pub page_type: crate::PageType,
    pub fact: String,
    pub why: String,
    pub how_to_apply: String,
    pub falsified_by: String,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitInspection {
    pub ok: bool,
    pub root: String,
    pub layout: LayoutClass,
    pub attainable: Vec<InitMode>,
    /// Preferred attainable mode. `full` when that mode is attainable, otherwise
    /// the only attainable mode, otherwise `null`.
    pub recommended_mode: Option<InitMode>,
    /// Opaque local approval token over the observations and pinned repository
    /// identity. Consumers must pass it back unchanged rather than recomputing it.
    pub inspection_sha256: String,
    pub dirty_paths: Vec<String>,
    pub prestates: Vec<NodePrestate>,
    pub conflicts: Vec<InitConflict>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitPlanRequest {
    pub root: String,
    pub inspection_sha256: String,
    pub mode: InitMode,
    pub date: String,
    /// Exact desired `AGENTS.md` bytes. Omit or leave empty to install or keep
    /// the canonical Project memory section when that is valid.
    #[serde(default)]
    pub agents_md: String,
    pub project_page: ProjectPageRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitManifest {
    pub manifest_contract: u32,
    pub layout_version: u32,
    pub yams_version: String,
    pub root: String,
    pub mode: InitMode,
    pub inspection_sha256: String,
    pub asset_sha256: BTreeMap<String, String>,
    pub operations: Vec<InitOperation>,
    /// SHA-256 of initialization-owned desired nodes only. This excludes the
    /// Yams runtime lock and unrelated repository nodes.
    pub candidate_sha256: String,
    pub proposal: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEnvelope {
    pub ok: bool,
    pub manifest_sha256: String,
    pub manifest: InitManifest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitOperation {
    pub kind: OperationKind,
    pub path: String,
    pub prestate: NodePrestate,
    pub mode: Option<u32>,
    pub content: Option<String>,
    pub post_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyResult {
    pub ok: bool,
    pub manifest_sha256: String,
    pub created: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub restored: Vec<String>,
    pub unresolved: Vec<String>,
    /// The observed final layout after safe inspection. `Partial` is also the
    /// conservative sentinel when a pre-access or early failure means apply
    /// could not validate a complete layout; it does not claim an observed
    /// partial repository in that case.
    pub final_layout: LayoutClass,
    pub validated: bool,
    pub error: Option<String>,
    /// Commands to run after a successful apply. Empty when apply did not
    /// finish a valid layout. These are hints; apply never runs them.
    pub next: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::*;

    fn project_page() -> ProjectPageRequest {
        ProjectPageRequest {
            title: "Repository initialization contracts".to_owned(),
            page_type: crate::PageType::ProjectState,
            fact: "Initialization uses an approved manifest.".to_owned(),
            why: "It makes repository mutations reviewable.".to_owned(),
            how_to_apply: "Inspect, plan, approve, and apply.".to_owned(),
            falsified_by: "An unapproved mutation succeeds.".to_owned(),
            summary: "Repository initialization is manifest-driven.".to_owned(),
        }
    }

    fn plan_request() -> InitPlanRequest {
        InitPlanRequest {
            root: "/fictional/repository".to_owned(),
            inspection_sha256: "inspection-digest".to_owned(),
            mode: InitMode::Full,
            date: "2026-08-12".to_owned(),
            agents_md: "# Agent policy\n".to_owned(),
            project_page: project_page(),
        }
    }

    fn prestate() -> NodePrestate {
        NodePrestate {
            path: ".agents/memory/index.md".to_owned(),
            kind: NodeKind::File,
            mode: Some(0o644),
            sha256: Some("before-digest".to_owned()),
            entries_sha256: Some("entries-digest".to_owned()),
        }
    }

    fn operation() -> InitOperation {
        InitOperation {
            kind: OperationKind::ReplaceFile,
            path: ".agents/memory/index.md".to_owned(),
            prestate: prestate(),
            mode: Some(0o644),
            content: Some("# Memory index\n".to_owned()),
            post_sha256: Some("after-digest".to_owned()),
        }
    }

    fn inspection() -> InitInspection {
        InitInspection {
            ok: false,
            root: "/fictional/repository".to_owned(),
            layout: LayoutClass::Partial,
            attainable: vec![InitMode::Minimal, InitMode::Full],
            recommended_mode: Some(InitMode::Full),
            inspection_sha256: "inspection-digest".to_owned(),
            dirty_paths: vec!["AGENTS.md".to_owned()],
            prestates: vec![prestate()],
            conflicts: vec![InitConflict {
                path: ".agents/memory".to_owned(),
                code: "fictional-conflict".to_owned(),
                detail: "The fictional node has an incompatible shape.".to_owned(),
            }],
        }
    }

    fn manifest() -> InitManifest {
        InitManifest {
            manifest_contract: 1,
            layout_version: 1,
            yams_version: "0.1.0".to_owned(),
            root: "/fictional/repository".to_owned(),
            mode: InitMode::Full,
            inspection_sha256: "inspection-digest".to_owned(),
            asset_sha256: BTreeMap::from([
                ("AGENTS.md".to_owned(), "agents-digest".to_owned()),
                ("index.md".to_owned(), "index-digest".to_owned()),
            ]),
            operations: vec![operation()],
            candidate_sha256: "candidate-digest".to_owned(),
            proposal: "Create the full repository memory layout.".to_owned(),
        }
    }

    fn envelope() -> ManifestEnvelope {
        ManifestEnvelope {
            ok: true,
            manifest_sha256: "manifest-digest".to_owned(),
            manifest: manifest(),
        }
    }

    #[test]
    fn init_plan_request_round_trips_all_fields() {
        let request = plan_request();

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["project_page"]["page_type"], "project-state");
        assert!(encoded["project_page"].get("type").is_none());
        assert_eq!(
            serde_json::from_value::<InitPlanRequest>(encoded).unwrap(),
            request
        );
    }

    #[test]
    fn manifest_envelope_round_trips_all_fields() {
        let envelope = envelope();

        let encoded = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            encoded["manifest"]["asset_sha256"]["AGENTS.md"],
            "agents-digest"
        );
        assert_eq!(
            encoded["manifest"]["operations"][0]["prestate"]["mode"],
            0o644
        );
        assert_eq!(
            serde_json::from_value::<ManifestEnvelope>(encoded).unwrap(),
            envelope
        );
    }

    #[test]
    fn init_inspection_round_trips_nonempty_findings() {
        let inspection = inspection();

        let encoded = serde_json::to_value(&inspection).unwrap();
        assert_eq!(encoded["attainable"], json!(["minimal", "full"]));
        assert_eq!(encoded["recommended_mode"], "full");
        assert_eq!(encoded["dirty_paths"], json!(["AGENTS.md"]));
        assert_eq!(encoded["prestates"][0]["kind"], "file");
        assert_eq!(encoded["conflicts"][0]["code"], "fictional-conflict");
        assert_eq!(
            serde_json::from_value::<InitInspection>(encoded).unwrap(),
            inspection
        );
    }

    #[test]
    fn apply_result_round_trips_nonempty_accounting() {
        let result = ApplyResult {
            ok: false,
            manifest_sha256: "manifest-digest".to_owned(),
            created: vec!["created".to_owned()],
            changed: vec!["changed".to_owned()],
            removed: vec!["removed".to_owned()],
            restored: vec!["restored".to_owned()],
            unresolved: vec!["unresolved".to_owned()],
            final_layout: LayoutClass::Partial,
            validated: false,
            error: Some("fictional apply failure".to_owned()),
            next: Vec::new(),
        };

        let encoded = serde_json::to_string(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<ApplyResult>(&encoded).unwrap(),
            result
        );
    }

    #[test]
    fn every_enum_variant_uses_exact_kebab_case_json() {
        let layout = [
            (LayoutClass::Absent, "absent"),
            (LayoutClass::Minimal, "minimal"),
            (LayoutClass::Full, "full"),
            (LayoutClass::Partial, "partial"),
        ];
        let modes = [(InitMode::Minimal, "minimal"), (InitMode::Full, "full")];
        let nodes = [
            (NodeKind::Missing, "missing"),
            (NodeKind::File, "file"),
            (NodeKind::Directory, "directory"),
            (NodeKind::Symlink, "symlink"),
            (NodeKind::Other, "other"),
        ];
        let operations = [
            (OperationKind::CreateDirectory, "create-directory"),
            (OperationKind::CreateFile, "create-file"),
            (OperationKind::ReplaceFile, "replace-file"),
            (OperationKind::RemoveFile, "remove-file"),
        ];

        for (value, expected) in layout {
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{expected}\"")
            );
            assert_eq!(
                serde_json::from_str::<LayoutClass>(&format!("\"{expected}\"")).unwrap(),
                value
            );
        }
        for (value, expected) in modes {
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{expected}\"")
            );
            assert_eq!(
                serde_json::from_str::<InitMode>(&format!("\"{expected}\"")).unwrap(),
                value
            );
        }
        for (value, expected) in nodes {
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{expected}\"")
            );
            assert_eq!(
                serde_json::from_str::<NodeKind>(&format!("\"{expected}\"")).unwrap(),
                value
            );
        }
        for (value, expected) in operations {
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{expected}\"")
            );
            assert_eq!(
                serde_json::from_str::<OperationKind>(&format!("\"{expected}\"")).unwrap(),
                value
            );
        }
    }

    fn assert_unknown_field_rejected<T>(value: Value)
    where
        T: serde::de::DeserializeOwned,
    {
        let error = match serde_json::from_value::<T>(value) {
            Ok(_) => panic!("unknown field was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_contract_layer() {
        let mut inspection_value = serde_json::to_value(inspection()).unwrap();
        inspection_value["unexpected"] = json!(true);
        assert_unknown_field_rejected::<InitInspection>(inspection_value);

        let mut conflict_value = serde_json::to_value(inspection()).unwrap();
        conflict_value["conflicts"][0]["unexpected"] = json!(true);
        assert_unknown_field_rejected::<InitInspection>(conflict_value);

        let mut request = serde_json::to_value(plan_request()).unwrap();
        request["unexpected"] = json!(true);
        assert_unknown_field_rejected::<InitPlanRequest>(request);

        let mut nested_page = serde_json::to_value(plan_request()).unwrap();
        nested_page["project_page"]["unexpected"] = json!(true);
        assert_unknown_field_rejected::<InitPlanRequest>(nested_page);

        let mut envelope_value = serde_json::to_value(envelope()).unwrap();
        envelope_value["unexpected"] = json!(true);
        assert_unknown_field_rejected::<ManifestEnvelope>(envelope_value);

        let mut manifest_value = serde_json::to_value(envelope()).unwrap();
        manifest_value["manifest"]["unexpected"] = json!(true);
        assert_unknown_field_rejected::<ManifestEnvelope>(manifest_value);

        let mut operation_value = serde_json::to_value(envelope()).unwrap();
        operation_value["manifest"]["operations"][0]["unexpected"] = json!(true);
        assert_unknown_field_rejected::<ManifestEnvelope>(operation_value);

        let mut prestate_value = serde_json::to_value(envelope()).unwrap();
        prestate_value["manifest"]["operations"][0]["prestate"]["unexpected"] = json!(true);
        assert_unknown_field_rejected::<ManifestEnvelope>(prestate_value);

        let mut result = serde_json::to_value(ApplyResult {
            ok: true,
            manifest_sha256: "manifest-digest".to_owned(),
            created: vec![],
            changed: vec![],
            removed: vec![],
            restored: vec![],
            unresolved: vec![],
            final_layout: LayoutClass::Full,
            validated: true,
            error: None,
            next: Vec::new(),
        })
        .unwrap();
        result["unexpected"] = json!(true);
        assert_unknown_field_rejected::<ApplyResult>(result);
    }

    #[test]
    fn required_fields_and_field_types_are_strict() {
        let mut missing_root = serde_json::to_value(plan_request()).unwrap();
        missing_root.as_object_mut().unwrap().remove("root");
        assert!(
            serde_json::from_value::<InitPlanRequest>(missing_root)
                .unwrap_err()
                .to_string()
                .contains("missing field `root`")
        );

        let mut omitted_policy = serde_json::to_value(plan_request()).unwrap();
        omitted_policy.as_object_mut().unwrap().remove("agents_md");
        assert_eq!(
            serde_json::from_value::<InitPlanRequest>(omitted_policy)
                .unwrap()
                .agents_md,
            ""
        );

        let mut wrong_contract_type = serde_json::to_value(envelope()).unwrap();
        wrong_contract_type["manifest"]["manifest_contract"] = json!("one");
        assert!(serde_json::from_value::<ManifestEnvelope>(wrong_contract_type).is_err());

        let mut missing_nested_path = serde_json::to_value(envelope()).unwrap();
        missing_nested_path["manifest"]["operations"][0]["prestate"]
            .as_object_mut()
            .unwrap()
            .remove("path");
        assert!(serde_json::from_value::<ManifestEnvelope>(missing_nested_path).is_err());
    }

    #[test]
    fn omitted_and_null_optional_fields_deserialize_as_none() {
        let mut omitted_prestate = serde_json::to_value(prestate()).unwrap();
        for field in ["mode", "sha256", "entries_sha256"] {
            omitted_prestate.as_object_mut().unwrap().remove(field);
        }
        let omitted_prestate = serde_json::from_value::<NodePrestate>(omitted_prestate).unwrap();
        assert_eq!(omitted_prestate.mode, None);
        assert_eq!(omitted_prestate.sha256, None);
        assert_eq!(omitted_prestate.entries_sha256, None);

        let mut null_prestate = serde_json::to_value(prestate()).unwrap();
        for field in ["mode", "sha256", "entries_sha256"] {
            null_prestate[field] = Value::Null;
        }
        let null_prestate = serde_json::from_value::<NodePrestate>(null_prestate).unwrap();
        assert_eq!(null_prestate.mode, None);
        assert_eq!(null_prestate.sha256, None);
        assert_eq!(null_prestate.entries_sha256, None);

        let mut omitted_operation = serde_json::to_value(operation()).unwrap();
        for field in ["mode", "content", "post_sha256"] {
            omitted_operation.as_object_mut().unwrap().remove(field);
        }
        let omitted_operation = serde_json::from_value::<InitOperation>(omitted_operation).unwrap();
        assert_eq!(omitted_operation.mode, None);
        assert_eq!(omitted_operation.content, None);
        assert_eq!(omitted_operation.post_sha256, None);

        let mut null_operation = serde_json::to_value(operation()).unwrap();
        for field in ["mode", "content", "post_sha256"] {
            null_operation[field] = Value::Null;
        }
        let null_operation = serde_json::from_value::<InitOperation>(null_operation).unwrap();
        assert_eq!(null_operation.mode, None);
        assert_eq!(null_operation.content, None);
        assert_eq!(null_operation.post_sha256, None);

        let apply_result = ApplyResult {
            ok: true,
            manifest_sha256: "manifest-digest".to_owned(),
            created: vec![],
            changed: vec![],
            removed: vec![],
            restored: vec![],
            unresolved: vec![],
            final_layout: LayoutClass::Full,
            validated: true,
            error: Some("fictional".to_owned()),
            next: Vec::new(),
        };
        let mut omitted_error = serde_json::to_value(&apply_result).unwrap();
        omitted_error.as_object_mut().unwrap().remove("error");
        assert_eq!(
            serde_json::from_value::<ApplyResult>(omitted_error)
                .unwrap()
                .error,
            None
        );
        let mut null_error = serde_json::to_value(apply_result).unwrap();
        null_error["error"] = Value::Null;
        assert_eq!(
            serde_json::from_value::<ApplyResult>(null_error)
                .unwrap()
                .error,
            None
        );
    }

    #[test]
    fn unsigned_integer_boundaries_are_enforced() {
        for invalid in [json!(-1), json!(u64::from(u32::MAX) + 1)] {
            let mut prestate_value = serde_json::to_value(prestate()).unwrap();
            prestate_value["mode"] = invalid.clone();
            assert!(serde_json::from_value::<NodePrestate>(prestate_value).is_err());

            for field in ["manifest_contract", "layout_version"] {
                let mut envelope_value = serde_json::to_value(envelope()).unwrap();
                envelope_value["manifest"][field] = invalid.clone();
                assert!(serde_json::from_value::<ManifestEnvelope>(envelope_value).is_err());
            }
        }
    }

    #[test]
    fn invalid_enum_spellings_are_rejected() {
        assert!(serde_json::from_str::<LayoutClass>(r#""project_state""#).is_err());
        assert!(serde_json::from_str::<InitMode>(r#""FULL""#).is_err());
        assert!(serde_json::from_str::<NodeKind>(r#""regular-file""#).is_err());
        assert!(serde_json::from_str::<OperationKind>(r#""replace_file""#).is_err());

        let mut invalid_page_type = serde_json::to_value(plan_request()).unwrap();
        invalid_page_type["project_page"]["page_type"] = json!("project_state");
        assert!(serde_json::from_value::<InitPlanRequest>(invalid_page_type).is_err());
    }

    #[test]
    fn project_page_type_key_is_exact_and_required() {
        let mut renamed = serde_json::to_value(plan_request()).unwrap();
        let page = renamed["project_page"].as_object_mut().unwrap();
        let page_type = page.remove("page_type").unwrap();
        page.insert("type".to_owned(), page_type);
        let renamed_error = serde_json::from_value::<InitPlanRequest>(renamed).unwrap_err();
        assert!(renamed_error.to_string().contains("unknown field `type`"));

        let mut missing = serde_json::to_value(plan_request()).unwrap();
        missing["project_page"]
            .as_object_mut()
            .unwrap()
            .remove("page_type");
        let missing_error = serde_json::from_value::<InitPlanRequest>(missing).unwrap_err();
        assert!(
            missing_error
                .to_string()
                .contains("missing field `page_type`")
        );
    }

    #[test]
    fn init_errors_expose_their_sources() {
        let source = serde_json::from_str::<InitPlanRequest>(r#"{"fictional":true}"#).unwrap_err();
        let expected_source = source.to_string();

        let error: super::super::InitError = source.into();

        assert_eq!(
            error.to_string(),
            format!("invalid initialization JSON: {expected_source}")
        );
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            expected_source
        );
        assert!(matches!(error, super::super::InitError::Json(_)));

        let io_error = super::super::InitError::Io {
            operation: "inspect fictional input",
            path: std::path::PathBuf::from("/fictional/repository"),
            source: std::io::Error::other("fictional I/O failure"),
        };
        assert_eq!(
            std::error::Error::source(&io_error).unwrap().to_string(),
            "fictional I/O failure"
        );
    }
}
