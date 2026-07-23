use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;
pub const METAL_SCHEMA: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfileManifest {
    pub schema: u32,
    pub name: String,
    pub protected: bool,
    pub created_at: String,
    pub source: ProfileSource,
    pub mappings: Vec<Mapping>,
    #[serde(default)]
    pub packages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileSource {
    Git {
        url: String,
        checkout: String,
        directory: PathBuf,
    },
    Captured {
        directory: PathBuf,
    },
    Builtin {
        directory: PathBuf,
    },
}

impl ProfileSource {
    pub fn directory(&self) -> &PathBuf {
        match self {
            Self::Git { directory, .. }
            | Self::Captured { directory }
            | Self::Builtin { directory } => directory,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Mapping {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Generation {
    pub schema: u32,
    pub id: String,
    pub profile: String,
    pub created_at: String,
    pub mappings: Vec<GenerationMapping>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenerationMapping {
    pub destination: PathBuf,
    /// Relative to the generation directory.
    pub object: PathBuf,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ActiveState {
    pub schema: u32,
    pub current: Option<Generation>,
    #[serde(default)]
    pub history: Vec<Generation>,
}

impl ActiveState {
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA,
            current: None,
            history: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BaselineIndex {
    pub schema: u32,
    pub entries: BTreeMap<PathBuf, BaselineEntry>,
}

impl BaselineIndex {
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BaselineEntry {
    pub present: bool,
    pub object: Option<PathBuf>,
    pub fingerprint: String,
    #[serde(default)]
    pub absent_parents: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    Prepared,
    FilesApplied,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Transaction {
    pub schema: u32,
    pub id: String,
    pub phase: TransactionPhase,
    pub old_active: ActiveState,
    pub new_active: ActiveState,
    pub operations: Vec<TransactionOperation>,
    #[serde(default)]
    pub created_parents: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TransactionOperation {
    pub destination: PathBuf,
    pub backup: PathBuf,
    pub original_present: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PackageState {
    pub schema: u32,
    /// Every package that appeared during a Dotlab pacman transaction,
    /// including dependencies.
    pub introduced: BTreeSet<String>,
    /// Explicit profile requirements that were absent before Dotlab.
    #[serde(default)]
    pub roots: BTreeSet<String>,
    /// Packages explicitly promoted to user ownership.
    pub kept: BTreeSet<String>,
}

impl PackageState {
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA,
            introduced: BTreeSet::new(),
            roots: BTreeSet::new(),
            kept: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MetalState {
    pub schema: u32,
    pub slots: BTreeMap<String, MetalSlot>,
}

impl MetalState {
    pub fn empty() -> Self {
        Self {
            schema: METAL_SCHEMA,
            slots: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetalSlot {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub root_snapshot: PathBuf,
    pub home_snapshot: PathBuf,
    pub root_subvol: String,
    pub home_subvol: String,
    pub slot_uki: PathBuf,
    pub original_uki: PathBuf,
    pub source_uki: PathBuf,
    pub kernel_arguments: Vec<String>,
    pub esp_uuid: String,
    #[serde(default)]
    pub promoted: bool,
    #[serde(default)]
    pub promoted_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingMetal {
    pub schema: u32,
    pub operation: MetalOperation,
    pub old_state: MetalState,
    pub new_state: MetalState,
    pub slot: MetalSlot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetalOperation {
    Create,
    Discard,
    Promote,
}
