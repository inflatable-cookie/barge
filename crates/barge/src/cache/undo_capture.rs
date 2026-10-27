//! Undo's durable, exclusive capture of the entries it is about to change.
//!
//! As with deletion, only a post-capture identity match authorises an unlink.
//! A mismatch is recorded before an exclusive put-back; a taken original name
//! never authorises replacement. Interrupted/failed journals are retained and
//! block another operation, rather than inferring completion from absence.

use super::*;

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Expected {
    Absent,
    File { snapshot: SettingsSnapshot },
    Link { device: u64, inode: u64 },
}

#[derive(Serialize)]
struct Effect {
    parent: PathBuf,
    parent_device: u64,
    parent_inode: u64,
    original: String,
    private: String,
    expected: Expected,
    captured: Option<(u64, u64, String, u32)>,
    publication: Option<SettingsSnapshot>,
    phase: String,
    put_back: String,
}

#[derive(Serialize)]
struct Journal {
    schema: &'static str,
    operation: &'static str,
    placement_id: String,
    effective_uid: u32,
    phase: String,
    effects: Vec<Effect>,
}

pub(super) struct Captures<'a> {
    state: &'a StatePaths,
    dirs: &'a dyn DirOps,
    kind: CacheKind,
    journal: Journal,
    parents: Vec<DirHandle>,
}

pub(super) fn journal_path(state: &StatePaths, kind: CacheKind) -> PathBuf {
    state
        .root()
        .join(format!("cache-undo-{}.json", kind.as_str()))
}

pub(super) fn require_no_pending(state: &StatePaths, kind: CacheKind) -> Result<()> {
    let path = journal_path(state, kind);
    if !matches!(
        state::read_bounded(
            &path,
            &Bounds::new(PLACEMENT_CAP_BYTES, 2, Deadline::after(READ_DEADLINE))
        ),
        ReadOutcome::Absent
    ) {
        return Err(unknown(
            "cache undo has a pending capture journal; preserve its objects for recovery",
        )
        .with_detail("pending_undo_intent", path.display().to_string().into()));
    }
    Ok(())
}

impl<'a> Captures<'a> {
    pub(super) fn new(
        state: &'a StatePaths,
        dirs: &'a dyn DirOps,
        record: &PlacementRecord,
        uid: u32,
    ) -> Self {
        Self {
            state,
            dirs,
            kind: record.kind,
            journal: Journal {
                schema: "barge.cache-undo/1",
                operation: "undo",
                placement_id: record.placement_id.clone(),
                effective_uid: uid,
                phase: "prepared".to_owned(),
                effects: Vec::new(),
            },
            parents: Vec::new(),
        }
    }

    pub(super) fn for_adopt(
        state: &'a StatePaths,
        dirs: &'a dyn DirOps,
        kind: CacheKind,
        uid: u32,
        operation: &'static str,
    ) -> Result<Self> {
        let intent = read_adopt_intent(state, kind)?
            .ok_or_else(|| unknown("capture requires a pending adoption intent"))?;
        Ok(Self {
            state,
            dirs,
            kind,
            journal: Journal {
                schema: "barge.cache-undo/1",
                operation,
                placement_id: intent.id,
                effective_uid: uid,
                phase: "prepared".to_owned(),
                effects: Vec::new(),
            },
            parents: Vec::new(),
        })
    }

    pub(super) fn absent_file(&mut self, path: &Path) -> Result<usize> {
        self.add(path, Expected::Absent)
    }

    pub(super) fn file(&mut self, path: &Path, snapshot: SettingsSnapshot) -> Result<usize> {
        self.add(path, Expected::File { snapshot })
    }

    pub(super) fn marker(&mut self, held: &HeldRoot, snapshot: SettingsSnapshot) -> Result<usize> {
        let index = self.file(&held.path.join(area::ROOT_MARKER_FILE), snapshot)?;
        let parent = self.parents[index].identity();
        if (parent.device.get(), parent.inode) != (held.device, held.inode) {
            return Err(unknown("held root parent changed before marker capture"));
        }
        Ok(index)
    }

    pub(super) fn link(&mut self, path: &Path, identity: (u64, u64)) -> Result<usize> {
        self.add(
            path,
            Expected::Link {
                device: identity.0,
                inode: identity.1,
            },
        )
    }

    fn add(&mut self, path: &Path, expected: Expected) -> Result<usize> {
        if self.parents.len() >= 2 {
            return Err(unknown("cache undo exceeds its two-effect bound"));
        }
        let (parent, original) = open_parent(self.dirs, path)?;
        let index = self.parents.len();
        let parent_identity = parent.identity();
        self.parents.push(parent);
        self.journal.effects.push(Effect {
            parent: path
                .parent()
                .ok_or_else(|| unknown("undo entry has no parent"))?
                .to_path_buf(),
            parent_device: parent_identity.device.get(),
            parent_inode: parent_identity.inode,
            original,
            private: format!(
                "{}{}",
                state::CAPTURE_PREFIX,
                hex_encode(&self.dirs.random_bytes(16)?)
            ),
            expected,
            captured: None,
            publication: None,
            phase: "planned".to_owned(),
            put_back: "not_attempted".to_owned(),
        });
        Ok(index)
    }

    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.journal)
            .map_err(|error| unknown(format!("cannot encode cache undo journal: {error}")))?;
        if bytes.len() > PLACEMENT_CAP_BYTES {
            return Err(unknown("cache undo journal exceeds its byte bound"));
        }
        state::publish_atomic(&journal_path(self.state, self.kind), &bytes)
    }

    fn verify(&self, index: usize) -> Result<()> {
        let effect = &self.journal.effects[index];
        let parent = &self.parents[index];
        if self.dirs.recheck_dir(parent)? != parent.identity() {
            return Err(unknown("undo capture parent binding changed"));
        }
        if matches!(effect.expected, Expected::Absent) {
            if self.dirs.child_facts(parent, &effect.original)?.is_some() {
                return Err(unknown("entry appeared before exclusive publication"));
            }
            return Ok(());
        }
        let facts = self
            .dirs
            .child_facts(parent, &effect.private)?
            .ok_or_else(|| unknown("undo capture disappeared; absence is not removal proof"))?;
        if facts.uid != self.journal.effective_uid {
            return Err(unknown("undo capture belongs to another account"));
        }
        match &effect.expected {
            Expected::Absent => Err(unknown("unexpected absent capture")),
            Expected::File { snapshot } => {
                if facts.kind != EntryKind::File {
                    return Err(unknown("undo captured a non-file"));
                }
                confirm_setting_at(self.dirs, parent, &effect.private, snapshot)
            }
            Expected::Link { device, inode } => {
                if facts.kind != EntryKind::Symlink
                    || (facts.device.get(), facts.inode) != (*device, *inode)
                {
                    return Err(unknown("undo captured a replaced link"));
                }
                Ok(())
            }
        }
    }

    /// Persist the planned names before any rename. Verify every captured
    /// effect before a caller may restore settings or unlink even one marker.
    pub(super) fn capture_all(&mut self) -> Result<()> {
        require_no_pending(self.state, self.kind)?;
        self.save()?;
        let result = self.capture_all_impl();
        if let Err(error) = result {
            self.journal.phase = format!("refused: {error}");
            // If recording fails, never put anything back without its record.
            self.save()?;
            self.put_back_all()?;
            return Err(error.with_detail(
                "pending_undo_intent",
                journal_path(self.state, self.kind)
                    .display()
                    .to_string()
                    .into(),
            ));
        }
        Ok(())
    }

    fn capture_all_impl(&mut self) -> Result<()> {
        for index in 0..self.parents.len() {
            if matches!(self.journal.effects[index].expected, Expected::Absent) {
                self.verify(index)?;
                continue;
            }
            let mut renamed = false;
            for _ in 0..8 {
                let effect = &self.journal.effects[index];
                if self.dirs.recheck_dir(&self.parents[index])? != self.parents[index].identity() {
                    return Err(unknown("undo capture parent changed before rename"));
                }
                match self.dirs.rename_child(
                    &self.parents[index],
                    &effect.original,
                    &effect.private,
                )? {
                    RenameOutcome::Renamed => {
                        renamed = true;
                        break;
                    }
                    RenameOutcome::TargetExists => {
                        self.journal.effects[index].private = format!(
                            "{}{}",
                            state::CAPTURE_PREFIX,
                            hex_encode(&self.dirs.random_bytes(16)?)
                        );
                        self.save()?;
                    }
                }
            }
            if !renamed {
                return Err(unknown("undo capture exhausted eight exclusive names"));
            }
            self.journal.effects[index].phase = "captured_unverified".to_owned();
            self.save()?;
            let effect = &self.journal.effects[index];
            self.journal.effects[index].captured = self
                .dirs
                .child_facts(&self.parents[index], &effect.private)?
                .map(|facts| {
                    (
                        facts.device.get(),
                        facts.inode,
                        facts.kind.as_str().to_owned(),
                        facts.uid,
                    )
                });
            self.verify(index)?;
            self.journal.effects[index].phase = "verified".to_owned();
            self.save()?;
        }
        for index in 0..self.parents.len() {
            self.verify(index)?;
            if self
                .dirs
                .child_facts(&self.parents[index], &self.journal.effects[index].original)?
                .is_some()
            {
                return Err(unknown("undo original name was taken while captured"));
            }
        }
        self.journal.phase = "all_captured_verified".to_owned();
        self.save()
    }

    fn put_back_all(&mut self) -> Result<()> {
        for index in (0..self.parents.len()).rev() {
            if self.journal.effects[index].phase == "planned"
                || matches!(self.journal.effects[index].expected, Expected::Absent)
            {
                continue;
            }
            self.journal.effects[index].put_back = "pending".to_owned();
            self.save()?;
            let effect = &self.journal.effects[index];
            let outcome =
                if self.dirs.recheck_dir(&self.parents[index])? != self.parents[index].identity() {
                    "parent_changed"
                } else {
                    match self.dirs.rename_child(
                        &self.parents[index],
                        &effect.private,
                        &effect.original,
                    ) {
                        Ok(RenameOutcome::Renamed) => "renamed",
                        Ok(RenameOutcome::TargetExists) => "taken",
                        Err(_) => "failed",
                    }
                };
            self.journal.effects[index].put_back = outcome.to_owned();
            self.save()?;
        }
        Ok(())
    }

    /// Publish restored bytes exclusively at the now-vacant original name;
    /// `publish_child_file` is used only for our recorded random staging name.
    pub(super) fn restore_file(
        &mut self,
        index: usize,
        bytes: &[u8],
        mode: u32,
    ) -> Result<SettingsSnapshot> {
        self.verify(index)?;
        let effect = &self.journal.effects[index];
        let stage = format!("{}.restore", effect.private);
        self.journal.phase = "publishing_restore".to_owned();
        self.save()?;
        self.dirs
            .publish_child_file(&self.parents[index], &stage, bytes, mode)?;
        let staged_facts = self
            .dirs
            .child_facts(&self.parents[index], &stage)?
            .ok_or_else(|| unknown("staged settings vanished"))?;
        let snapshot = snapshot_of(&staged_facts, bytes)?;
        confirm_setting_at(self.dirs, &self.parents[index], &stage, &snapshot)?;
        self.journal.effects[index].publication = Some(snapshot.clone());
        self.save()?;
        let effect = &self.journal.effects[index];
        match self
            .dirs
            .rename_child(&self.parents[index], &stage, &effect.original)?
        {
            RenameOutcome::Renamed => {}
            RenameOutcome::TargetExists => {
                return Err(unknown(
                    "settings original name was taken; restored bytes and captures kept",
                ))
            }
        }
        self.journal.effects[index].phase = "restore_published".to_owned();
        self.save()?;
        Ok(snapshot)
    }

    pub(super) fn return_unchanged(&mut self, index: usize) -> Result<()> {
        self.verify(index)?;
        let effect = &self.journal.effects[index];
        match self
            .dirs
            .rename_child(&self.parents[index], &effect.private, &effect.original)?
        {
            RenameOutcome::Renamed => {}
            RenameOutcome::TargetExists => {
                return Err(unknown("unchanged settings name was taken; captures kept"))
            }
        }
        self.journal.effects[index].phase = "returned_unchanged".to_owned();
        self.save()
    }

    pub(super) fn remove(&mut self, index: usize) -> Result<()> {
        self.verify(index)?;
        self.journal.phase = "removing_verified_capture".to_owned();
        self.save()?;
        let effect = &self.journal.effects[index];
        match self
            .dirs
            .remove_child(&self.parents[index], &effect.private, RemoveKind::File)?
        {
            crate::os::ChildRemoval::Removed => {}
            _ => {
                return Err(unknown(
                    "undo capture removal did not positively remove its object",
                ))
            }
        }
        self.journal.effects[index].phase = "removed".to_owned();
        self.save()
    }

    pub(super) fn finish(&mut self) -> Result<()> {
        self.journal.phase = "complete".to_owned();
        self.save()?;
        remove_record(self.state, &self.journal.placement_id)?;
        self.settle()
    }

    pub(super) fn settle(&mut self) -> Result<()> {
        self.journal.phase = "complete".to_owned();
        self.save()?;
        std::fs::remove_file(journal_path(self.state, self.kind))
            .map_err(|error| unknown(format!("cannot remove completed undo journal: {error}")))?;
        std::fs::File::open(self.state.root())
            .and_then(|file| file.sync_all())
            .map_err(|error| unknown(format!("cannot sync completed undo journal: {error}")))
    }
}
