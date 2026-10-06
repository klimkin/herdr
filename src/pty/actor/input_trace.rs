//! Private Unix actor diagnostics; no input bytes or published codecs change.

#[derive(Debug, Default)]
pub(super) struct InputQueueTrace {
    scope: u64,
    // One incomplete controlled marker; ordinary input retains no bytes.
    tail: [(u8, u64); 14],
    len: usize,
}

#[derive(Debug, Default)]
pub(super) struct CommandTrace {
    pub text: PartTrace,
    pub enter: PartTrace,
    id: u64,
    scope: u64,
    bytes: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PartTrace {
    id: u64,
    scope: u64,
    bytes: u64,
}

impl InputQueueTrace {
    pub(super) fn accepted(&mut self, text: &[u8], enter: Option<&[u8]>) -> CommandTrace {
        let ns = crate::latency_prof::now();
        if ns == 0 {
            return CommandTrace::default();
        }
        if self.scope == 0 {
            self.scope = crate::latency_prof::next_scope();
        }
        let id = crate::latency_prof::next_scope();
        let bytes = text.len() as u64 + enter.map_or(0, |bytes| bytes.len() as u64);
        let trace = CommandTrace {
            id,
            scope: self.scope,
            bytes,
            text: PartTrace {
                id,
                scope: self.scope,
                bytes: text.len() as u64,
            },
            enter: enter.map_or_else(PartTrace::default, |bytes| PartTrace {
                id: crate::latency_prof::next_scope(),
                scope: self.scope,
                bytes: bytes.len() as u64,
            }),
        };
        self.part(text, trace.text, id);
        if let Some(enter) = enter {
            self.part(enter, trace.enter, id);
        }
        trace
    }

    fn part(&mut self, bytes: &[u8], part: PartTrace, command: u64) {
        crate::latency_prof::record_at(
            "input.actor_part",
            part.id,
            command,
            self.scope,
            crate::latency_prof::now(),
        );
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if self.len == 0 {
                let Some(start) = remaining.iter().position(|byte| *byte == b'!') else {
                    break;
                };
                remaining = &remaining[start..];
            }
            let byte = remaining[0];
            remaining = &remaining[1..];
            if byte == b'!' {
                self.tail[0] = (byte, part.id);
                self.len = 1;
                continue;
            }
            let valid = if self.len == 13 {
                byte == b'~'
            } else {
                byte.is_ascii_hexdigit()
            };
            if !valid {
                self.len = 0;
                continue;
            }
            self.tail[self.len] = (byte, part.id);
            self.len += 1;
            if self.len == 14 {
                let identity = self.tail[1..13].iter().fold(0u64, |value, &(byte, _)| {
                    let digit = match byte {
                        b'0'..=b'9' => byte - b'0',
                        b'a'..=b'f' => byte - b'a' + 10,
                        _ => byte - b'A' + 10,
                    };
                    value * 16 + u64::from(digit)
                });
                let mut previous = 0;
                for &(_, contributor) in &self.tail {
                    if contributor != previous {
                        crate::latency_prof::record_at(
                            "input.actor_fragment",
                            identity,
                            contributor,
                            self.scope,
                            crate::latency_prof::now(),
                        );
                        previous = contributor;
                    }
                }
                self.len = 0;
            }
        }
    }
}

impl CommandTrace {
    pub(super) fn enqueue(&self) {
        self.record("input.actor_enqueue");
    }
    pub(super) fn claim(&self) {
        self.record("input.actor_claim");
    }
    pub(super) fn discard(&self) {
        self.record("input.actor_discard");
    }
    fn record(&self, stage: &'static str) {
        if self.id != 0 {
            crate::latency_prof::record_at(
                stage,
                self.id,
                self.bytes,
                self.scope,
                crate::latency_prof::now(),
            );
        }
    }
}

impl PartTrace {
    pub(super) fn is_empty(self) -> bool {
        self.bytes == 0
    }
    pub(super) fn record(self, stage: &'static str) {
        if self.id != 0 && self.bytes != 0 {
            crate::latency_prof::record_at(
                stage,
                self.id,
                self.bytes,
                self.scope,
                crate::latency_prof::now(),
            );
        }
    }
}
