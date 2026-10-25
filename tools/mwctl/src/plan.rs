//! The upload request sequence.
//!
//! [`upload_plan`] turns an image into the requests that install it, the
//! same sequence the web page sends (protocol-wasm's `uploadPlan`):
//! `sys/loader/begin` with `[len][crc32]`, then `sys/loader/chunk` with
//! `[offset][data]` for each in-order chunk of at most
//! [`CHUNK_MAX`] bytes, then `sys/loader/commit`. Bodies are written by
//! [`Command::encode_body`], so they match what the device parses.
//!
//! The `corrupt` test hook flips every bit of the first data byte of one
//! chunk after the whole-image CRC-32 has been computed from the original
//! image. Every frame is still valid on the wire and every chunk is
//! accepted, but the device's running CRC differs from the declared one, so
//! `sys/loader/commit` answers `InvalidRequest`.

use middleware::Key;
use middleware::crc::Crc32;
use middleware::loader::{CHUNK_MAX, Command};
use std::fmt;

/// One request of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Request key.
    pub key: Key<'static>,
    /// Request body.
    pub body: Vec<u8>,
    /// Image bytes delivered once this step is acknowledged.
    pub done: u32,
}

/// Why no plan could be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// The image is empty.
    Empty,
    /// The image does not fit the protocol's 32-bit length.
    TooLarge(usize),
    /// `--corrupt-chunk` names a chunk the image does not have.
    NoSuchChunk {
        /// Requested chunk index.
        index: usize,
        /// Number of chunks in the plan.
        chunks: usize,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Empty => f.write_str("the payload image is empty"),
            PlanError::TooLarge(n) => write!(f, "the payload image is too large ({n} bytes)"),
            PlanError::NoSuchChunk { index, chunks } => write!(
                f,
                "--corrupt-chunk {index}: the image has only {chunks} chunk(s) (indices 0..{})",
                chunks.saturating_sub(1)
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Number of chunks an image of `len` bytes is sent in.
pub const fn chunk_count(len: usize) -> usize {
    len.div_ceil(CHUNK_MAX)
}

fn step(cmd: &Command<'_>, done: u32) -> Step {
    let mut body = vec![0u8; 8 + CHUNK_MAX];
    // `body` holds the largest body (`[offset][CHUNK_MAX bytes]`).
    let n = cmd.encode_body(&mut body).unwrap_or(0);
    body.truncate(n);
    Step {
        key: cmd.key(),
        body,
        done,
    }
}

/// The requests that install `image`, with chunk `corrupt` (zero-based)
/// altered after the CRC is computed, when given.
pub fn upload_plan(image: &[u8], corrupt: Option<usize>) -> Result<Vec<Step>, PlanError> {
    if image.is_empty() {
        return Err(PlanError::Empty);
    }
    let len = u32::try_from(image.len()).map_err(|_| PlanError::TooLarge(image.len()))?;
    let chunks = chunk_count(image.len());
    if let Some(index) = corrupt.filter(|&i| i >= chunks) {
        return Err(PlanError::NoSuchChunk { index, chunks });
    }
    let crc = Crc32::checksum(image);
    let mut steps = Vec::with_capacity(chunks + 2);
    steps.push(step(&Command::Begin { len, crc }, 0));
    let mut offset = 0u32;
    let mut scratch = [0u8; CHUNK_MAX];
    for (i, data) in image.chunks(CHUNK_MAX).enumerate() {
        let data = if corrupt == Some(i) {
            let part = scratch
                .get_mut(..data.len())
                .ok_or(PlanError::TooLarge(data.len()))?;
            part.copy_from_slice(data);
            if let Some(b) = part.first_mut() {
                *b ^= 0xFF;
            }
            &*part
        } else {
            data
        };
        // `data.len() <= CHUNK_MAX` and the offsets sum to `len`.
        let end = offset + data.len() as u32;
        steps.push(step(&Command::Chunk { offset, data }, end));
        offset = end;
    }
    steps.push(step(&Command::Commit, len));
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::{Effect, Loader, keys};

    fn image(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + 7) as u8).collect()
    }

    /// Runs a plan through the device's loader state machine.
    fn run(steps: &[Step]) -> Result<Effect<'static>, middleware::HandlerError> {
        let mut s = Loader::Idle;
        let mut last = Ok(Effect::None);
        for st in steps {
            let cmd = Command::parse(st.key, &st.body)
                .expect("parses")
                .expect("system key");
            let (next, e) = s.step(&cmd, 1 << 20);
            s = next;
            last = e.map(|e| match e {
                Effect::Activate { len, crc } => Effect::Activate { len, crc },
                _ => Effect::None,
            });
            last?;
        }
        last
    }

    #[test]
    fn plan_has_begin_chunks_commit_and_runs_through_the_loader() {
        let img = image(1300);
        let steps = upload_plan(&img, None).unwrap();
        assert_eq!(steps.len(), 1 + 3 + 1);
        assert_eq!(steps[0].key, keys::BEGIN);
        assert_eq!(steps[0].body.len(), 8);
        assert_eq!(steps[1].key, keys::CHUNK);
        assert_eq!(steps[1].body.len(), 4 + CHUNK_MAX);
        assert_eq!(steps[3].body.len(), 4 + 1300 - 2 * CHUNK_MAX);
        assert_eq!(steps[4].key, keys::COMMIT);
        assert!(steps[4].body.is_empty());
        let done: Vec<u32> = steps.iter().map(|s| s.done).collect();
        assert_eq!(done, [0, 512, 1024, 1300, 1300]);
        assert_eq!(
            run(&steps),
            Ok(Effect::Activate {
                len: 1300,
                crc: Crc32::checksum(&img)
            })
        );
    }

    #[test]
    fn corrupt_chunk_keeps_declared_crc_and_fails_commit() {
        let img = image(1300);
        let good = upload_plan(&img, None).unwrap();
        let bad = upload_plan(&img, Some(1)).unwrap();
        assert_eq!(good[0], bad[0], "begin declares the original CRC");
        assert_ne!(good[2].body, bad[2].body);
        assert_eq!(good[2].body.len(), bad[2].body.len());
        assert_eq!(good[1], bad[1]);
        assert_eq!(
            run(&bad),
            Err(middleware::HandlerError::InvalidRequest),
            "commit rejects the CRC"
        );
    }

    #[test]
    fn rejects_empty_images_and_missing_chunks() {
        assert_eq!(upload_plan(&[], None), Err(PlanError::Empty));
        assert_eq!(
            upload_plan(&image(600), Some(2)),
            Err(PlanError::NoSuchChunk {
                index: 2,
                chunks: 2
            })
        );
        assert!(upload_plan(&image(600), Some(1)).is_ok());
        assert_eq!(chunk_count(512), 1);
        assert_eq!(chunk_count(513), 2);
    }
}
