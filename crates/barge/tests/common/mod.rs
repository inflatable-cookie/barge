//! Shared fixture: a temporary directory, a fake volume and a manual clock.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use barge::os::FakeOs;
use barge::testing::TempDir;
use barge::{Area, AreaSpec, Barge, ManualClock, VolumeRegistration};

pub const UUID: &str = "11111111-2222-3333-4444-555555555555";
pub const OTHER_UUID: &str = "99999999-8888-7777-6666-555555555555";
pub const GIB: u64 = 1024 * 1024 * 1024;
pub const TIB: u64 = 1024 * GIB;

pub struct Fixture {
    /// Held for its lifetime: dropping it removes every fixture file.
    pub temp: TempDir,
    pub mount: PathBuf,
    pub os: Arc<FakeOs>,
    pub clock: Arc<ManualClock>,
    pub barge: Barge,
    pub reserve: u64,
    pub floor: u64,
    pub root_dir: String,
}

impl Fixture {
    pub fn new() -> Self {
        let temp = TempDir::new("lean-core").expect("a temporary directory");
        let mount = temp.folder("Builds").expect("a mount folder");
        let state = temp.folder("state").expect("a state folder");
        let os = Arc::new(FakeOs::new());
        os.add_volume(mount.clone(), Some(UUID), TIB);
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let barge = Barge::open_with(state, os.clone(), clock.clone()).expect("open Barge");
        Self {
            temp,
            mount,
            os,
            clock,
            barge,
            reserve: 0,
            floor: 0,
            root_dir: String::new(),
        }
    }

    pub fn register(&self) {
        self.barge
            .register_volume(
                VolumeRegistration::new("Builds", self.mount.clone())
                    .root_dir(self.root_dir.clone())
                    .reserve_bytes(self.reserve)
                    .floor_bytes(self.floor),
            )
            .expect("register the volume");
    }

    pub fn base(&self) -> PathBuf {
        if self.root_dir.is_empty() {
            self.mount.clone()
        } else {
            self.mount.join(&self.root_dir)
        }
    }

    pub fn area(&self, name: &str) -> Area {
        self.barge
            .area(AreaSpec::new("test", name, "Builds"))
            .expect("declare the area")
    }

    pub fn area_dir(&self, name: &str) -> PathBuf {
        self.base().join("test").join(name)
    }
}

pub fn fill(root: &barge::Root, bytes: usize) {
    fs::write(root.path.join("data.bin"), vec![b'x'; bytes]).expect("write root data");
}
