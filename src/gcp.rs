//! Google Cloud TPU VM adapter. All external work crosses Runner; no SDK
//! credential files, account keys, or tokens are copied to a VM.
pub(crate) mod catalog;
pub(crate) mod expiry;
pub(crate) mod facts;
pub(crate) mod gcloud;
pub(crate) mod geo;
pub(crate) mod http;
pub(crate) mod id;
pub(crate) mod inventory;
pub(crate) mod launch;
pub(crate) mod lifecycle;
pub(crate) mod preferences;
pub(crate) mod prices;
pub(crate) mod retry;
pub(crate) mod settings;
pub(crate) mod ssh;
pub(crate) mod startup;
pub(crate) mod types;
pub(crate) mod wizard;
pub(crate) mod zones;

pub use gcloud::{bad_reply, json, objects, run, text};
pub(crate) use gcloud::{json_timeout, run_timeout};
pub(crate) use id::resource_parts;
pub use id::{component, TpuId};
pub use inventory::{all_nodes, list, nodes};
pub(crate) use inventory::{find_node, node_id, vm_node};
pub(crate) use launch::{confirmation, deadline_label, launch, launch_prepared, Prepared, TpuArgs};
pub use launch::{launch_from_picker, launch_setup, unique_name};
pub use lifecycle::lifecycle;
pub use settings::Settings;
