use std::collections::BTreeMap;

#[derive(Debug)]
pub struct Observation {
    pub marker: String,
    pub received_ns: u64,
}

/// Persistent terminal state observes effects across partial redraws.
pub struct Observer {
    terminal: ghostty_vt::Terminal,
    pending: BTreeMap<String, u64>,
    commit_prefix: usize,
    cols: u16,
    rows: u16,
    pub newest_load_generation: Option<u64>,
    pub load_observations: Vec<(String, u64, u64)>,
    load_generations: BTreeMap<String, u64>,
}

impl Observer {
    pub fn new(cols: u16, rows: u16) -> Result<Self, ghostty_vt::Error> {
        Ok(Self {
            terminal: ghostty_vt::Terminal::new(cols, rows, 0)?,
            pending: BTreeMap::new(),
            commit_prefix: 0,
            cols,
            rows,
            newest_load_generation: None,
            load_observations: Vec::new(),
            load_generations: BTreeMap::new(),
        })
    }

    pub fn expect(&mut self, marker: String, sent_ns: u64) {
        self.pending.insert(marker, sent_ns);
    }

    pub fn cancel(&mut self, marker: &str) {
        self.pending.remove(marker);
    }

    pub fn text(&self) -> Result<String, ghostty_vt::Error> {
        self.terminal
            .read_text_viewport((0, 0), (self.cols - 1, u32::from(self.rows - 1)), true)
    }

    pub fn feed(
        &mut self,
        bytes: &[u8],
        received_ns: u64,
    ) -> Result<Vec<Observation>, ghostty_vt::Error> {
        let mut observed = Vec::new();
        // Split only at actual synchronized-output commits, including a
        // terminator split across reads. Ordinary text remains batch-parsed.
        const COMMIT: &[u8] = b"\x1b[?2026l";
        let mut start = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == COMMIT[self.commit_prefix] {
                self.commit_prefix += 1;
            } else {
                self.commit_prefix = usize::from(*byte == COMMIT[0]);
            }
            if self.commit_prefix == COMMIT.len() {
                self.terminal.write(&bytes[start..=index]);
                observed.extend(self.collect(received_ns)?);
                self.commit_prefix = 0;
                start = index + 1;
            }
        }
        self.terminal.write(&bytes[start..]);
        observed.extend(self.collect(received_ns)?);
        Ok(observed)
    }

    fn collect(&mut self, received_ns: u64) -> Result<Vec<Observation>, ghostty_vt::Error> {
        if self.terminal.mode_get(2026)? {
            return Ok(Vec::new());
        }
        let text = self.text()?;
        for tail in text.split("LOAD-").skip(1) {
            let Some((pane, tail)) = tail.split_once('-') else {
                continue;
            };
            if !pane.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                continue;
            }
            let Some(generation) = tail.get(..10).and_then(|text| text.parse::<u64>().ok()) else {
                continue;
            };
            if self
                .load_generations
                .get(pane)
                .is_none_or(|previous| generation > *previous)
            {
                self.load_generations.insert(pane.to_owned(), generation);
                self.load_observations
                    .push((pane.to_owned(), generation, received_ns));
                self.newest_load_generation =
                    Some(self.newest_load_generation.unwrap_or(0).max(generation));
            }
        }
        let matched = self
            .pending
            .iter()
            .filter(|(marker, sent)| {
                received_ns >= **sent
                    && text.contains(marker.as_str())
                    && !(marker.starts_with('A') && text.contains("rename workspace"))
            })
            .map(|(marker, _)| marker.clone())
            .collect::<Vec<_>>();
        Ok(matched
            .into_iter()
            .map(|marker| {
                self.pending.remove(&marker);
                Observation {
                    marker,
                    received_ns,
                }
            })
            .collect())
    }
}
