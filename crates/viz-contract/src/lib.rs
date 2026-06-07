//! viz-contract — the ViewMusic artifact contract: typed model, schema export,
//! and the load/validation pipeline.
//!
//! The single published contract every visual artifact conforms to.
//! [`types`] holds the serde + schemars model that mirrors the published reference
//! schema at `docs/reference/artifact.schema.json`; the schemars `JsonSchema` derive
//! regenerates a schema kept semantically equivalent to that reference. The contract
//! is documented in full at `docs/reference/artifact-contract.md`.

pub mod library;
pub mod load;
pub mod types;

#[doc(inline)]
pub use types::{
    limits, Artifact, Blend, CellBlock, ColorSpec, ElementBlock, Feedback, Formula, Layer, Meta,
    PointBlock, SettingDecl, Shape, VarDecl,
};

#[doc(inline)]
pub use load::{
    load_artifact, published_schema, ArtifactPrograms, ColorModel, ColorPrograms, CompiledFormula,
    LayerPrograms, LoadDiagnostic, LoadStep, LoadedArtifact, SettingSlots, SlotLayout,
    SHARED_INPUTS, STAGE_EXTRAS,
};

#[doc(inline)]
pub use library::{
    ArtifactLibrary, ArtifactSource, EntryStatus, LibraryEntry, ARTIFACT_FILE_SUFFIX,
};
