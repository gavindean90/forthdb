fn decode_file(bytes: &[u8]) -> Result<(Vec<Arc<CommitFrame>>, usize), FileCommitStoreError> {
    if bytes.len() < FILE_HEADER_LEN {
        return Err(FileCommitStoreError::InvalidHeader(
            "header is incomplete".to_owned(),
        ));
    }
    if &bytes[..8] != FILE_MAGIC {
        return Err(FileCommitStoreError::InvalidHeader(
            "magic bytes do not match FTHDB001".to_owned(),
        ));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed header slice"));
    if version != FILE_VERSION {
        return Err(FileCommitStoreError::UnsupportedFormat(version));
    }
    let flags = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed header slice"));
    if flags != FILE_FLAGS {
        return Err(FileCommitStoreError::InvalidHeader(format!(
            "unsupported header flags 0x{flags:08x}"
        )));
    }

    let mut frames = Vec::new();
    let mut offset = FILE_HEADER_LEN;
    let mut last_good_offset = FILE_HEADER_LEN;

    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        if remaining < FRAME_PREFIX_LEN {
            break;
        }
        if &bytes[offset..offset + 4] != FRAME_MAGIC {
            return Err(corrupt(offset, "frame magic does not match FRM1"));
        }

        let payload_len = u64::from_le_bytes(
            bytes[offset + 4..offset + 12]
                .try_into()
                .expect("fixed frame length slice"),
        );
        if payload_len > MAX_FRAME_BYTES {
            return Err(corrupt(
                offset,
                format!("declared payload length {payload_len} exceeds limit"),
            ));
        }
        let payload_len = usize::try_from(payload_len)
            .map_err(|_| corrupt(offset, "payload length does not fit this platform"))?;
        let total_len = FRAME_PREFIX_LEN
            .checked_add(payload_len)
            .and_then(|value| value.checked_add(FRAME_TRAILER_LEN))
            .ok_or_else(|| corrupt(offset, "record length overflow"))?;
        if remaining < total_len {
            break;
        }

        let expected_checksum = u64::from_le_bytes(
            bytes[offset + 12..offset + 20]
                .try_into()
                .expect("fixed checksum slice"),
        );
        let payload_start = offset + FRAME_PREFIX_LEN;
        let payload_end = payload_start + payload_len;
        let trailer_end = payload_end + FRAME_TRAILER_LEN;
        if &bytes[payload_end..trailer_end] != FRAME_TRAILER {
            return Err(corrupt(offset, "completion trailer does not match END1"));
        }

        let payload = &bytes[payload_start..payload_end];
        let actual_checksum = checksum(payload);
        if actual_checksum != expected_checksum {
            return Err(corrupt(
                offset,
                format!(
                    "checksum mismatch: expected {expected_checksum:016x}, calculated {actual_checksum:016x}"
                ),
            ));
        }

        let frame = Arc::new(
            decode_payload(payload)
                .map_err(|reason| corrupt(offset, format!("invalid payload: {reason}")))?,
        );
        validate_decoded_position(&frames, &frame, offset)?;
        frames.push(frame);
        offset += total_len;
        last_good_offset = offset;
    }

    Ok((frames, last_good_offset))
}

fn validate_decoded_position(
    frames: &[Arc<CommitFrame>],
    frame: &CommitFrame,
    offset: usize,
) -> Result<(), FileCommitStoreError> {
    let (expected_parent, expected_parent_version) = frames
        .last()
        .map(|current| (current.resulting_world(), current.resulting_version()))
        .unwrap_or((WorldId::GENESIS, 0));

    if frame.parent_world() != expected_parent {
        return Err(corrupt(
            offset,
            format!(
                "expected parent {expected_parent}, found {}",
                frame.parent_world()
            ),
        ));
    }
    if frame.parent_version() != expected_parent_version {
        return Err(corrupt(
            offset,
            format!(
                "expected parent version {expected_parent_version}, found {}",
                frame.parent_version()
            ),
        ));
    }
    let expected_resulting_version = expected_parent_version
        .checked_add(1)
        .ok_or_else(|| corrupt(offset, "world version overflow"))?;
    if frame.resulting_version() != expected_resulting_version {
        return Err(corrupt(
            offset,
            format!(
                "expected resulting version {expected_resulting_version}, found {}",
                frame.resulting_version()
            ),
        ));
    }
    Ok(())
}

fn corrupt(offset: usize, reason: impl Into<String>) -> FileCommitStoreError {
    FileCommitStoreError::CorruptFrame {
        offset: offset as u64,
        reason: reason.into(),
    }
}

fn decode_payload(payload: &[u8]) -> Result<CommitFrame, String> {
    let mut decoder = Decoder::new(payload);
    let parent_world = WorldId::new(decoder.u64()?);
    let resulting_world = WorldId::new(decoder.u64()?);
    let parent_version = decoder.u64()?;
    let resulting_version = decoder.u64()?;
    let resulting_allocator = decoder.u64()?;
    let operation_count = decoder.u64()?;
    if operation_count > MAX_OPERATIONS_PER_FRAME {
        return Err(format!(
            "operation count {operation_count} exceeds limit {MAX_OPERATIONS_PER_FRAME}"
        ));
    }

    let mut operations = Vec::with_capacity(operation_count as usize);
    for _ in 0..operation_count {
        let operation = match decoder.byte()? {
            0 => Operation::AllocateEntity {
                entity: EntityId::new(decoder.u64()?),
            },
            1 => Operation::Define {
                slot: SlotId::new(decoder.string()?),
                fact: Fact::new(decoder.atom()?, Predicate::new(decoder.string()?), decoder.atom()?),
            },
            2 => Operation::Forget {
                slot: SlotId::new(decoder.string()?),
            },
            tag => return Err(format!("unknown operation tag {tag}")),
        };
        operations.push(operation);
    }
    decoder.finish()?;

    Ok(CommitFrame {
        parent_world,
        resulting_world,
        parent_version,
        resulting_version,
        resulting_allocator,
        operations: Arc::from(operations),
    })
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut value = CHECKSUM_OFFSET_BASIS;
    for byte in bytes {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(CHECKSUM_PRIME);
    }
    value
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "payload cursor overflow".to_owned())?;
        if end > self.bytes.len() {
            return Err(format!(
                "unexpected end of payload at byte {}, need {length} more bytes",
                self.position
            ));
        }
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .expect("decoder requested exactly eight bytes"),
        ))
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.u64()?;
        let length = usize::try_from(length)
            .map_err(|_| "string length does not fit this platform".to_owned())?;
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|error| format!("string is not valid UTF-8: {error}"))?;
        Ok(value.to_owned())
    }

    fn atom(&mut self) -> Result<Atom, String> {
        match self.byte()? {
            0 => Ok(Atom::Entity(EntityId::new(self.u64()?))),
            1 => Ok(Atom::Literal(Literal::new(self.string()?))),
            tag => Err(format!("unknown atom tag {tag}")),
        }
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "{} trailing payload bytes remain",
                self.bytes.len() - self.position
            ))
        }
    }
}
