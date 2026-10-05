//! Native decoding and reloadable, bounded packet policy are separate boundaries.
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_PACKET_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_PACKETS: usize = 128;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecoderConfig {
    pub decoder: String,
    pub source: String,
    pub frequency_hz: u64,
    pub sample_rate_hz: u32,
}

impl DecoderConfig {
    pub fn validate(&self) -> Result<(), String> {
        super::validate_id("decoder", &self.decoder)?;
        super::validate_id("decoder source", &self.source)?;
        if self.frequency_hz == 0 || self.sample_rate_hz == 0 {
            return Err("frequency and sample rate must be positive".into());
        }
        Ok(())
    }
}

/// Owner-defined decoded data. Reception metadata cannot be rewritten by Lua.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodedPacket {
    pub source: String,
    pub session_id: String,
    pub sequence: u64,
    pub observed_at_unix_ms: u64,
    pub contract: String,
    pub value: Value,
}

impl DecodedPacket {
    pub fn validate(&self) -> Result<(), String> {
        super::validate_id("packet source", &self.source)?;
        super::validate_id("packet session", &self.session_id)?;
        super::validate_id("packet contract", &self.contract)?;
        if self.sequence == 0 || self.observed_at_unix_ms == 0 || !self.value.is_object() {
            return Err("packet needs positive sequence/timestamp and object value".into());
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_PACKET_BYTES {
            return Err("decoded packet exceeds 64 KiB".into());
        }
        Ok(())
    }
}

/// Implementations own native DSP. Host arbitration must precede `start`.
/// `process` must return at most MAX_BATCH_PACKETS; the host must reject excess.
/// No implementation or hardware access is implied by this interface.
pub trait Decoder {
    fn start(&mut self, config: &DecoderConfig) -> Result<(), String>;
    fn process(&mut self, iq: &[u8]) -> Result<Vec<DecodedPacket>, String>;
    fn stop(&mut self) -> Result<(), String>;
}

/// Validated native lifecycle wrapper. Device arbitration remains host-owned.
pub struct DecoderSession<D> {
    decoder: D,
    config: Option<DecoderConfig>,
}

impl<D: Decoder> DecoderSession<D> {
    pub fn new(decoder: D) -> Self {
        Self {
            decoder,
            config: None,
        }
    }

    pub fn start(&mut self, config: DecoderConfig) -> Result<(), String> {
        if self.config.is_some() {
            return Err("decoder already running".into());
        }
        config.validate()?;
        self.decoder.start(&config)?;
        self.config = Some(config);
        Ok(())
    }

    pub fn process(&mut self, iq: &[u8]) -> Result<Vec<DecodedPacket>, String> {
        let config = self.config.as_ref().ok_or("decoder is not running")?;
        // Bound each native call; the receiver can split larger blocks explicitly.
        if iq.len() > 1024 * 1024 {
            return Err("IQ block exceeds 1 MiB".into());
        }
        let packets = self.decoder.process(iq)?;
        if packets.len() > MAX_BATCH_PACKETS {
            return Err("decoder batch exceeds packet limit".into());
        }
        for packet in &packets {
            packet.validate()?;
            if packet.source != config.source {
                return Err("decoder emitted wrong source".into());
            }
        }
        Ok(packets)
    }

    pub fn stop(&mut self) -> Result<(), String> {
        if self.config.is_none() {
            return Err("decoder is not running".into());
        }
        self.decoder.stop()?;
        self.config = None;
        Ok(())
    }
}

#[cfg(feature = "lua")]
pub mod lua {
    use super::*;
    use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, StdLib, Table, VmState};

    const MEMORY_LIMIT: usize = 4 * 1024 * 1024;
    const INSTRUCTION_LIMIT: u32 = 100_000;

    /// A dedicated VM: no filesystem, OS, package loading, or bus capabilities.
    /// Hooks return one stream ID or nil (drop); the Rust host owns publishing.
    pub struct PacketPolicy {
        lua: Lua,
        config: DecoderConfig,
    }

    impl PacketPolicy {
        pub fn load(script: &str) -> mlua::Result<Self> {
            if script.len() > MAX_PACKET_BYTES {
                return Err(mlua::Error::external("policy script exceeds 64 KiB"));
            }
            let lua = Lua::new_with(
                StdLib::TABLE | StdLib::STRING | StdLib::MATH,
                Default::default(),
            )?;
            lua.set_memory_limit(MEMORY_LIMIT)?;
            // Disable protected calls so scripts cannot swallow resource-limit errors.
            lua.globals().set("pcall", mlua::Value::Nil)?;
            lua.globals().set("xpcall", mlua::Value::Nil)?;
            Self::budget(&lua)?;
            let module: Table = lua.load(script).eval()?;
            let config: DecoderConfig = lua.from_value(module.get("config")?)?;
            config.validate().map_err(mlua::Error::external)?;
            let hook: Function = module.get("on_packet")?;
            lua.globals().set("__radioman_packet_hook", hook)?;
            Ok(Self { lua, config })
        }

        fn budget(lua: &Lua) -> mlua::Result<()> {
            lua.set_hook(
                HookTriggers::new().every_nth_instruction(INSTRUCTION_LIMIT),
                |_, _| {
                    Err::<VmState, _>(mlua::Error::external(
                        "Lua packet policy instruction limit exceeded",
                    ))
                },
            )
        }

        pub fn config(&self) -> &DecoderConfig {
            &self.config
        }

        /// Reload only policy-compatible configuration. Retuning/restarting is explicit.
        /// Invalid candidates leave this VM and configuration untouched.
        pub fn reload(&mut self, script: &str) -> mlua::Result<()> {
            let candidate = Self::load(script)?;
            if candidate.config != self.config {
                return Err(mlua::Error::external(
                    "decoder configuration changed; host restart required",
                ));
            }
            *self = candidate;
            Ok(())
        }

        pub fn route(&self, packet: &DecodedPacket) -> mlua::Result<Option<String>> {
            packet.validate().map_err(mlua::Error::external)?;
            if packet.source != self.config.source {
                return Err(mlua::Error::external(
                    "packet source differs from configured decoder source",
                ));
            }
            Self::budget(&self.lua)?;
            let hook: Function = self.lua.globals().get("__radioman_packet_hook")?;
            let stream: Option<String> = hook.call(self.lua.to_value(packet)?)?;
            if let Some(id) = &stream {
                super::super::validate_id("output stream", id).map_err(mlua::Error::external)?;
            }
            Ok(stream)
        }
    }
}
