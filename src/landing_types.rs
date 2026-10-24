//! Message values shared by the landing dispatcher and its Java backend.
//! This module owns no world state, threads, callbacks, or backend selection.

#[derive(Clone, Copy, Debug)]
pub enum Loader {
    Fabric,
    NeoForge,
}

#[derive(Clone, Copy, Debug)]
pub struct Trample {
    pub position: (i32, i32, i32),
    pub state: u32,
    // read by the Java adapter (java_compat.rs passes it as a JNI float), so it
    // is only dead in builds without that feature
    #[cfg_attr(not(feature = "java-compat"), allow(dead_code))]
    pub distance: f32,
}

#[cfg(feature = "java-compat")]
pub struct Request {
    pub trample: Trample,
    pub reply: std::sync::mpsc::SyncSender<Result<bool, String>>,
}
