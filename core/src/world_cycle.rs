// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::world_cycle` — the Vanilla 1.21.1 world-cycle rules as pure,
//! deterministic, sans-IO state machines: the weather cycle, natural lightning,
//! sleeping with its insomnia side effect, and the client-visible weather game
//! events that make rain and thunder actually render.
//!
//! Why this module exists: the tick loop and the client both need these rules,
//! and every previous copy of them (an `advanceWeatherCycle`-shaped function in
//! the server, a rain-level ramp in the client, an insomnia counter per player)
//! drifted. This module owns the rules, takes the world state as arguments, and
//! spends randomness through an explicit [`IntSampler`], so nothing here reads a
//! clock, a global, or a thread-local. Two runs with the same inputs and the same
//! draws produce the same output — which is what makes the tests below possible.
//!
//! Verified against the pinned 1.21.1 jar (`tools/vanilla_data/`):
//!
//! * `ServerLevel.<clinit>`: `RAIN_DELAY (12000, 180000)`, `RAIN_DURATION
//!   (12000, 24000)`, `THUNDER_DELAY (12000, 180000)`, `THUNDER_DURATION
//!   (3600, 15600)`.
//! * `ServerLevel.advanceWeatherCycle`: independent rain and thunder timers (the
//!   thunder timer keeps counting while it is not raining), the
//!   `clearWeatherTime` branch that pins both timers to `0`/`1`, `±0.01` level
//!   ramps clamped to `[0, 1]`, and the `ClientboundGameEventPacket` broadcasts
//!   `START_RAINING (1)`, `STOP_RAINING (2)`, `RAIN_LEVEL_CHANGE (7)` and
//!   `THUNDER_LEVEL_CHANGE (8)` — the last two are the P0 fix: without them a
//!   vanilla client never ramps its own rain and thunder levels and renders
//!   clear skies during a storm.
//! * `ServerLevel.tickChunk`: lightning is `isRaining && isThundering &&
//!   random.nextInt(100_000) == 0`, then `findLightningTargetAround` (which
//!   searches `128` blocks for a lightning rod), `isRainingAt`, and a skeleton
//!   trap when `doMobSpawning && random.nextDouble() <
//!   effectiveDifficulty * 0.01` and the block below is not a lightning rod.
//! * `ServerPlayer.startSleepInBed` / `Level.isDay` / `Level.updateSkyBrightness`
//!   / `DimensionType.timeOfDay` / `LevelTimeAccess.getTimeOfDay`: sleeping is
//!   refused while `skyDarken < 4`, which is exactly the audited windows
//!   `12542..=23459` clear, `12010..=23991` raining, always while thundering —
//!   [`VanillaSleepState::is_sleep_time`] is that window table and
//!   [`VanillaSleepState::is_sleep_time_with_levels`] is the underlying
//!   `skyDarken` derivation, and the tests pin them against each other.
//! * `SleepStatus.sleepersNeeded` / `areEnoughSleeping`: `max(1, ceil(active *
//!   pct / 100f))`, with `playersSleepingPercentage > 100` meaning "never skip".
//! * `PhantomSpawner.tick`: `72000` ticks since rest, a `* 20`-ticked cooldown of
//!   `60 + nextInt(60)`, and a `nextInt(ticksSinceRest) >= 72000` roll.
//!
//! Every type here is `Copy` or trivially cloneable, none of them touch a global,
//! and none of them allocate: [`WeatherTickEvents`] is a fixed three-slot buffer
//! because one tick can broadcast at most three game events.

use crate::math::Mth;

// ---------- weather constants (ServerLevel.<clinit>) ----------

/// `ServerLevel.RAIN_DELAY` — the inclusive span of ticks of clear weather the
/// cycle samples before it starts raining.
pub const RAIN_DELAY: (i32, i32) = (12_000, 180_000);

/// `ServerLevel.RAIN_DURATION` — the inclusive span of ticks of rain the cycle
/// samples before it clears up.
pub const RAIN_DURATION: (i32, i32) = (12_000, 24_000);

/// `ServerLevel.THUNDER_DELAY` — sampled while it is *not* thundering, and
/// independently of the rain timer.
pub const THUNDER_DELAY: (i32, i32) = (12_000, 180_000);

/// `ServerLevel.THUNDER_DURATION` — sampled while it *is* thundering.
pub const THUNDER_DURATION: (i32, i32) = (3_600, 15_600);

/// `0.01F` per tick: how much `Level.rainLevel` moves toward `1.0`/`0.0`.
pub const RAIN_LEVEL_CHANGE_PER_TICK: f32 = 0.01;

/// `0.01F` per tick: how much `Level.thunderLevel` moves toward `1.0`/`0.0`.
pub const THUNDER_LEVEL_CHANGE_PER_TICK: f32 = 0.01;

/// `Level.isRaining()`: `getRainLevel(1.0F) > 0.2`. Note this is the *ramped*
/// level, not the stored flag — that mismatch is why `START_RAINING` lands on
/// the tick the ramp crosses `0.2` rather than the tick the timer expired.
pub const RAIN_LEVEL_VISIBLE_THRESHOLD: f32 = 0.2;

/// `Level.isThundering()`: `getThunderLevel(1.0F) > 0.9` (plus the dimension's
/// sky-light/ceiling gates, which are the caller's to check).
pub const THUNDER_LEVEL_VISIBLE_THRESHOLD: f32 = 0.9;

/// `ClientboundGameEventPacket.START_RAINING`.
pub const GAME_EVENT_START_RAINING: u8 = 1;

/// `ClientboundGameEventPacket.STOP_RAINING`.
pub const GAME_EVENT_STOP_RAINING: u8 = 2;

/// `ClientboundGameEventPacket.RAIN_LEVEL_CHANGE`.
pub const GAME_EVENT_RAIN_LEVEL_CHANGE: u8 = 7;

/// `ClientboundGameEventPacket.THUNDER_LEVEL_CHANGE`.
pub const GAME_EVENT_THUNDER_LEVEL_CHANGE: u8 = 8;

/// The most game events one weather tick can broadcast: a rain level change, a
/// thunder level change and one start/stop transition.
pub const MAX_EVENTS_PER_TICK: usize = 3;

// ---------- randomness ----------

/// The one draw the world cycle spends: `RandomSource.nextInt(bound)`.
///
/// Implemented for `&mut FnMut(i32) -> i32` so tests can pin an exact draw, and
/// for the crate's Java-exact generators ([`crate::random::Xoroshiro128pp`],
/// which is what `ServerLevel.random` is, and [`crate::random::LegacyRandom`]).
/// Using a `next(bits)` closure instead would force this module to re-implement
/// each generator's rejection loop, which is how draw counts start to differ.
pub trait IntSampler {
    /// A uniform draw in `0..bound`, exactly `RandomSource.nextInt(bound)`.
    ///
    /// # Panics
    ///
    /// Vanilla throws `IllegalArgumentException` for `bound <= 0`.
    fn next_int_bounded(&mut self, bound: i32) -> i32;
}

impl<F: FnMut(i32) -> i32> IntSampler for F {
    fn next_int_bounded(&mut self, bound: i32) -> i32 {
        (*self)(bound)
    }
}

impl IntSampler for crate::random::Xoroshiro128pp {
    fn next_int_bounded(&mut self, bound: i32) -> i32 {
        crate::random::Xoroshiro128pp::next_int_bounded(self, bound)
    }
}

impl IntSampler for crate::random::LegacyRandom {
    fn next_int_bounded(&mut self, bound: i32) -> i32 {
        crate::random::LegacyRandom::next_int_bounded(self, bound)
    }
}

/// `Mth.nextInt(random, min, max)` — an inclusive sample from an
/// `IntProvider.create(min, max)` span.
///
/// A sampler that returns an out-of-range draw is clamped into the span instead
/// of being trusted: the span is a contract the callers below (and the tests
/// above them) rely on for timer bounds.
#[must_use]
fn sample_inclusive(rng: &mut impl IntSampler, span: (i32, i32)) -> i32 {
    let (min, max) = span;
    let bound = max.saturating_sub(min).saturating_add(1);
    if bound <= 0 {
        return min;
    }
    min + Mth::clamp(rng.next_int_bounded(bound), 0, bound - 1)
}

// ---------- weather game events ----------

/// A `ClientboundGameEventPacket` the weather cycle broadcasts.
///
/// The variant order is the order vanilla broadcasts them in a single tick:
/// rain level, thunder level, then the start/stop transition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WeatherGameEvent {
    /// `START_RAINING (1)` — the ramped rain level crossed `0.2` upward.
    StartRaining,
    /// `STOP_RAINING (2)` — the ramped rain level fell to `0.2` or below.
    StopRaining,
    /// `RAIN_LEVEL_CHANGE (7)` carrying the new rain level, broadcast on every
    /// tick the ramp moved. This is the packet that makes a vanilla client
    /// render rain.
    RainLevelChange(f32),
    /// `THUNDER_LEVEL_CHANGE (8)` carrying the new thunder level.
    ThunderLevelChange(f32),
}

impl WeatherGameEvent {
    /// The `ClientboundGameEventPacket` type id this event is sent as.
    #[must_use]
    pub const fn id(self) -> u8 {
        match self {
            Self::StartRaining => GAME_EVENT_START_RAINING,
            Self::StopRaining => GAME_EVENT_STOP_RAINING,
            Self::RainLevelChange(_) => GAME_EVENT_RAIN_LEVEL_CHANGE,
            Self::ThunderLevelChange(_) => GAME_EVENT_THUNDER_LEVEL_CHANGE,
        }
    }

    /// The `float` parameter the packet carries, or `None` for the two
    /// transition events (vanilla sends `0.0F` there).
    #[must_use]
    pub const fn level(self) -> Option<f32> {
        match self {
            Self::StartRaining | Self::StopRaining => None,
            Self::RainLevelChange(level) | Self::ThunderLevelChange(level) => Some(level),
        }
    }
}

/// The game events one [`VanillaWeatherState::tick`] produced, in broadcast
/// order.
///
/// At most three: a rain level change, a thunder level change and one
/// transition. It is a fixed buffer rather than a `Vec` because the tick path
/// must not allocate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WeatherTickEvents {
    buffer: [WeatherGameEvent; MAX_EVENTS_PER_TICK],
    len: usize,
}

impl WeatherTickEvents {
    /// No events.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: [WeatherGameEvent::StartRaining; MAX_EVENTS_PER_TICK],
            len: 0,
        }
    }

    /// Append an event. The buffer cannot overflow: a tick emits at most one
    /// event per kind, and there are three kinds.
    fn push(&mut self, event: WeatherGameEvent) {
        if self.len < MAX_EVENTS_PER_TICK {
            self.buffer[self.len] = event;
            self.len += 1;
        }
    }

    /// The events that were emitted.
    #[must_use]
    pub fn as_slice(&self) -> &[WeatherGameEvent] {
        &self.buffer[..self.len]
    }

    /// How many events this tick emitted.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this tick emitted nothing — the common case.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether any emitted event carries `id`.
    #[must_use]
    pub fn contains_id(&self, id: u8) -> bool {
        self.as_slice().iter().any(|event| event.id() == id)
    }

    /// Iterate the events in broadcast order.
    pub fn iter(&self) -> impl Iterator<Item = &WeatherGameEvent> {
        self.as_slice().iter()
    }
}

impl Default for WeatherTickEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> IntoIterator for &'a WeatherTickEvents {
    type Item = &'a WeatherGameEvent;
    type IntoIter = std::slice::Iter<'a, WeatherGameEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

// ---------- the weather cycle ----------

/// The Vanilla 1.21.1 weather cycle: the four timers, the two stored flags and
/// the two ramped levels a `ServerLevel` keeps.
///
/// The timers and flags are the fields `level.dat` persists
/// (`clearWeatherTime`, `rainTime`, `thunderTime`, `raining`, `thundering`);
/// the levels are runtime state (`rainLevel`, `thunderLevel`) that vanilla does
/// not save and re-ramps from zero after a reload.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VanillaWeatherState {
    clear_weather_time: i32,
    rain_time: i32,
    thunder_time: i32,
    raining: bool,
    thundering: bool,
    rain_level: f32,
    thunder_level: f32,
}

impl VanillaWeatherState {
    /// A level that has never ticked: clear, both levels at `0.0`, every timer
    /// at `0`.
    ///
    /// The first [`tick`](Self::tick) with `do_weather_cycle` therefore samples
    /// `RAIN_DELAY`, which is what a freshly created world does.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            clear_weather_time: 0,
            rain_time: 0,
            thunder_time: 0,
            raining: false,
            thundering: false,
            rain_level: 0.0,
            thunder_level: 0.0,
        }
    }

    /// Restore the persisted half of the state from `level.dat`.
    ///
    /// The levels are deliberately not restorable: vanilla re-ramps them from
    /// `0.0` on load, and starting a reloaded thunderstorm at `1.0` would make
    /// the first broadcast disagree with a vanilla client's own state.
    #[must_use]
    pub const fn from_saved(
        clear_weather_time: i32,
        rain_time: i32,
        thunder_time: i32,
        raining: bool,
        thundering: bool,
    ) -> Self {
        Self {
            clear_weather_time,
            rain_time,
            thunder_time,
            raining,
            thundering,
            rain_level: 0.0,
            thunder_level: 0.0,
        }
    }

    /// `/weather clear [duration]`: `setWeatherParameters(duration, 0, false,
    /// false)`.
    /// Note that the cleared flags and the pinned timers mean precipitation
    /// resumes on the first tick after the countdown expires — vanilla keeps the
    /// rain *state* in the timer, not a remembered intent.
    pub fn set_clear(&mut self, duration: i32) {
        self.clear_weather_time = duration.max(0);
        self.rain_time = 0;
        self.thunder_time = 0;
        self.raining = false;
        self.thundering = false;
    }

    /// `/weather rain [duration]`: `setWeatherParameters(0, duration, true,
    /// false)`. Note that vanilla sets *both* timers to `duration`; they still
    /// run down independently from there.
    pub fn set_rain(&mut self, duration: i32) {
        self.clear_weather_time = 0;
        self.rain_time = duration.max(0);
        self.thunder_time = duration.max(0);
        self.raining = true;
        self.thundering = false;
    }

    /// `/weather thunder [duration]`: `setWeatherParameters(0, duration, true,
    /// true)`.
    pub fn set_thunder(&mut self, duration: i32) {
        self.clear_weather_time = 0;
        self.rain_time = duration.max(0);
        self.thunder_time = duration.max(0);
        self.raining = true;
        self.thundering = true;
    }

    /// `ServerLevelData.getClearWeatherTime`.
    #[must_use]
    pub const fn clear_weather_time(&self) -> i32 {
        self.clear_weather_time
    }

    /// `ServerLevelData.getRainTime`.
    #[must_use]
    pub const fn rain_time(&self) -> i32 {
        self.rain_time
    }

    /// `ServerLevelData.getThunderTime`.
    #[must_use]
    pub const fn thunder_time(&self) -> i32 {
        self.thunder_time
    }

    /// `ServerLevelData.isRaining` — the stored flag the timers flip, which is
    /// what drives the ramp, **not** what `Level.isRaining()` answers.
    #[must_use]
    pub const fn is_raining(&self) -> bool {
        self.raining
    }

    /// `ServerLevelData.isThundering`.
    #[must_use]
    pub const fn is_thundering(&self) -> bool {
        self.thundering
    }

    /// `Level.getRainLevel(1.0F)`.
    #[must_use]
    pub const fn rain_level(&self) -> f32 {
        self.rain_level
    }

    /// `Level.getThunderLevel(1.0F)`.
    #[must_use]
    pub const fn thunder_level(&self) -> f32 {
        self.thunder_level
    }

    /// `Level.isRaining()`: the ramped rain level passed `0.2`.
    #[must_use]
    pub fn is_raining_visible(&self) -> bool {
        self.rain_level > RAIN_LEVEL_VISIBLE_THRESHOLD
    }

    /// `Level.isThundering()`: the ramped thunder level passed `0.9`. The
    /// dimension gates (`hasSkyLight`, `hasCeiling`) are the caller's.
    #[must_use]
    pub fn is_thundering_visible(&self) -> bool {
        self.thunder_level > THUNDER_LEVEL_VISIBLE_THRESHOLD
    }

    /// One `ServerLevel.advanceWeatherCycle`.
    ///
    /// * `do_weather_cycle` is `gamerules.doWeatherCycle()`; without it the
    ///   timers freeze but the levels still ramp toward the current flags.
    /// * `has_sky_light` is `dimensionType().hasSkyLight()`; without it vanilla
    ///   advances nothing at all — an underground dimension never rains.
    ///
    /// `rng` is only drawn from when a timer has to be resampled, matching
    /// vanilla's draw count exactly.
    pub fn tick(
        &mut self,
        do_weather_cycle: bool,
        has_sky_light: bool,
        rng: &mut impl IntSampler,
    ) -> WeatherTickEvents {
        let mut events = WeatherTickEvents::new();
        let was_raining_visible = self.is_raining_visible();

        if has_sky_light {
            if do_weather_cycle {
                if self.clear_weather_time > 0 {
                    // `/weather clear` pins both timers to 0/1 while it holds the
                    // sky clear, so the moment it expires the timer resolves into
                    // a fresh delay instead of a stale one.
                    self.clear_weather_time -= 1;
                    self.thunder_time = i32::from(!self.thundering);
                    self.rain_time = i32::from(!self.raining);
                    self.thundering = false;
                    self.raining = false;
                } else {
                    // The two timers are independent: thunder keeps counting down
                    // while it is not raining and can start during clear weather.
                    if self.thunder_time > 0 {
                        self.thunder_time -= 1;
                        if self.thunder_time == 0 {
                            self.thundering = !self.thundering;
                        }
                    } else if self.thundering {
                        self.thunder_time = sample_inclusive(rng, THUNDER_DURATION);
                    } else {
                        self.thunder_time = sample_inclusive(rng, THUNDER_DELAY);
                    }

                    if self.rain_time > 0 {
                        self.rain_time -= 1;
                        if self.rain_time == 0 {
                            self.raining = !self.raining;
                        }
                    } else if self.raining {
                        self.rain_time = sample_inclusive(rng, RAIN_DURATION);
                    } else {
                        self.rain_time = sample_inclusive(rng, RAIN_DELAY);
                    }
                }
            }

            let previous_rain_level = self.rain_level;
            let rain_step = if self.raining {
                RAIN_LEVEL_CHANGE_PER_TICK
            } else {
                -RAIN_LEVEL_CHANGE_PER_TICK
            };
            // `f32` accumulation is the reference behaviour: 20 ticks of
            // `+0.01F` land one ulp above `0.2F`, which is why `START_RAINING`
            // fires on the twentieth tick and not the twenty-first.
            self.rain_level = Mth::clamp(self.rain_level + rain_step, 0.0, 1.0);

            let previous_thunder_level = self.thunder_level;
            let thunder_step = if self.thundering {
                THUNDER_LEVEL_CHANGE_PER_TICK
            } else {
                -THUNDER_LEVEL_CHANGE_PER_TICK
            };
            self.thunder_level = Mth::clamp(self.thunder_level + thunder_step, 0.0, 1.0);

            if self.rain_level != previous_rain_level {
                events.push(WeatherGameEvent::RainLevelChange(self.rain_level));
            }
            if self.thunder_level != previous_thunder_level {
                events.push(WeatherGameEvent::ThunderLevelChange(self.thunder_level));
            }
        }

        if was_raining_visible != self.is_raining_visible() {
            if was_raining_visible {
                events.push(WeatherGameEvent::StopRaining);
            } else {
                events.push(WeatherGameEvent::StartRaining);
            }
        }

        events
    }
}

impl Default for VanillaWeatherState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------- natural lightning ----------

/// `ServerLevel.tickChunk`: `random.nextInt(100_000) == 0`.
pub const LIGHTNING_CHUNK_ROLL_BOUND: u32 = 100_000;

/// `ServerLevel.findLightningRod`: `PoiManager.findClosest(..., 128, ...)` — a
/// lightning rod within this many blocks of the strike point takes the bolt.
pub const LIGHTNING_ROD_SEARCH_RADIUS: i32 = 128;

/// `ServerLevel.tickChunk`: the skeleton-trap roll is
/// `nextDouble() < (double) effectiveDifficulty * 0.01`, so the comparison is a
/// `double` one even though the difficulty itself is a `float`.
pub const SKELETON_TRAP_CHANCE_PER_DIFFICULTY: f64 = 0.01;

/// The Vanilla 1.21.1 natural-lightning rules.
pub struct NaturalLightningPolicy;

impl NaturalLightningPolicy {
    /// Whether a thunderstorm strikes inside the chunk being ticked.
    ///
    /// `roll_100_000` is `random.nextInt(LIGHTNING_CHUNK_ROLL_BOUND)`; vanilla
    /// additionally requires the chunk to be raining at the target block
    /// (`Level.isRainingAt`), which the caller checks against the struck
    /// position because it depends on the heightmap and biome precipitation.
    #[must_use]
    pub const fn should_strike_chunk(
        is_raining: bool,
        is_thundering: bool,
        roll_100_000: u32,
    ) -> bool {
        is_raining && is_thundering && roll_100_000 == 0
    }

    /// `DifficultyInstance.isHarderThan(float)` — `effectiveDifficulty > other`.
    #[must_use]
    pub fn difficulty_is_harder_than(local_difficulty: f32, other: f32) -> bool {
        local_difficulty > other
    }

    /// The skeleton-trap horse probability for a local difficulty:
    /// `(double) effectiveDifficulty * 0.01`.
    #[must_use]
    pub fn skeleton_trap_horse_chance(local_difficulty: f32) -> f64 {
        f64::from(local_difficulty) * SKELETON_TRAP_CHANCE_PER_DIFFICULTY
    }

    /// The full `ServerLevel.tickChunk` skeleton-trap gate:
    /// `doMobSpawning && nextDouble() < chance && !blockBelow.is(LIGHTNING_ROD)`.
    ///
    /// `roll` is `random.nextDouble()`; a roll outside `[0, 1)` is clamped, so a
    /// broken generator cannot fabricate a trap.
    #[must_use]
    pub fn should_spawn_skeleton_trap(
        do_mob_spawning: bool,
        local_difficulty: f32,
        roll: f64,
        lightning_rod_below: bool,
    ) -> bool {
        if !do_mob_spawning || lightning_rod_below {
            return false;
        }
        Mth::clamp(roll, 0.0, 1.0) < Self::skeleton_trap_horse_chance(local_difficulty)
    }
}

// ---------- sleeping ----------

/// `24000` ticks: one full day, the modulus the sleep window is expressed in.
pub const DAY_LENGTH: i64 = 24_000;

/// `Level.isDay()`: `skyDarken < 4` (with no fixed time).
pub const SKY_DARKEN_DAY_THRESHOLD: i32 = 4;

/// The clear-weather sleep window, inclusive: `12542..=23459`.
///
/// This is `!Level.isDay()` while `rainLevel == 0.0 && thunderLevel == 0.0`;
/// `12541` is still day and `23460` is day again.
pub const SLEEP_TIME_CLEAR: (i64, i64) = (12_542, 23_459);

/// The raining sleep window, inclusive: `12010..=23991`. Rain darkens the sky,
/// so the window widens on both ends.
pub const SLEEP_TIME_RAIN: (i64, i64) = (12_010, 23_991);

/// `ServerPlayer.startSleepInBed`: the `Monster` query around the bed is
/// `AABB(bed center).inflate(8, 5, 8)`.
pub const MONSTER_CHECK_RADIUS_XZ: f64 = 8.0;

/// Vertical half-extent of the bed's monster query.
pub const MONSTER_CHECK_RADIUS_Y: f64 = 5.0;

/// The axis-aligned box `ServerPlayer.startSleepInBed` queries for
/// `Monster`s before allowing sleep, in `Level.getEntitiesOfClass` terms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonsterCheckBox {
    /// Minimum corner, inclusive.
    pub min: [f64; 3],
    /// Maximum corner, exclusive for [`contains_point`](Self::contains_point).
    pub max: [f64; 3],
}

impl MonsterCheckBox {
    /// The box around `bed` (the bed block's own position, at the bed's
    /// bottom-center) that vanilla inspects for monsters.
    #[must_use]
    pub fn around_bed(bed_x: i32, bed_y: i32, bed_z: i32) -> Self {
        // `Vec3.atBottomCenterOf(pos)`: +0.5 on X and Z, no Y offset.
        let center_x = f64::from(bed_x) + 0.5;
        let center_y = f64::from(bed_y);
        let center_z = f64::from(bed_z) + 0.5;
        Self {
            min: [
                center_x - MONSTER_CHECK_RADIUS_XZ,
                center_y - MONSTER_CHECK_RADIUS_Y,
                center_z - MONSTER_CHECK_RADIUS_XZ,
            ],
            max: [
                center_x + MONSTER_CHECK_RADIUS_XZ,
                center_y + MONSTER_CHECK_RADIUS_Y,
                center_z + MONSTER_CHECK_RADIUS_XZ,
            ],
        }
    }

    /// `AABB.contains(x, y, z)`: half-open — the minimum corner counts, the
    /// maximum does not, exactly as vanilla's `AABB` does.
    #[must_use]
    pub fn contains_point(&self, x: f64, y: f64, z: f64) -> bool {
        x >= self.min[0]
            && x < self.max[0]
            && y >= self.min[1]
            && y < self.max[1]
            && z >= self.min[2]
            && z < self.max[2]
    }

    /// `AABB.intersects(other)`: the strict test entity bounding boxes go
    /// through, which is what `getEntitiesOfClass` actually applies.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        self.min[0] < other.max[0]
            && self.max[0] > other.min[0]
            && self.min[1] < other.max[1]
            && self.max[1] > other.min[1]
            && self.min[2] < other.max[2]
            && self.max[2] > other.min[2]
    }
}

/// The Vanilla 1.21.1 sleeping rules, as pure functions.
///
/// The state they describe — who is in bed, where the bed is, how many players
/// the dimension holds — belongs to the caller; nothing in here is stored.
pub struct VanillaSleepState;

impl VanillaSleepState {
    /// `Mth.frac(double)`: `d - Math.floor(d)`.
    fn frac(value: f64) -> f64 {
        value - value.floor()
    }

    /// `DimensionType.timeOfDay(long)`: the celestial angle in `0.0..1.0` that
    /// [`sky_darken`] feeds into the cosine.
    #[must_use]
    pub fn dimension_time_of_day(day_time: i64) -> f32 {
        let day_fraction = Self::frac(day_time as f64 / 24_000.0 - 0.25);
        let squashed = 0.5 - (day_fraction * std::f64::consts::PI).cos() / 2.0;
        ((day_fraction * 2.0 + squashed) as f32) / 3.0
    }

    /// `Level.updateSkyBrightness`: `skyDarken` in `0..=11`, the number
    /// `Level.isDay()` compares against `4`.
    ///
    /// `Level.getTimeOfDay(1.0F)` ignores its partial-tick argument, so the day
    /// time alone decides the angle. The rain and thunder terms are the *ramped*
    /// levels, not the stored flags — that is why a thunderstorm without rain
    /// stays `skyDarken == 3` at noon and cannot be slept through, while a
    /// thunderstorm with rain reaches `5`.
    #[must_use]
    pub fn sky_darken(day_time: i64, rain_level: f32, thunder_level: f32) -> i32 {
        let rain_factor = 1.0 - f64::from(rain_level) * 5.0 / 16.0;
        let thunder_factor = 1.0 - f64::from(thunder_level) * 5.0 / 16.0;
        let angle = Self::dimension_time_of_day(day_time) * 6.283_185_5;
        let celestial = 0.5 + 2.0 * Mth::clamp(f64::from(Mth::cos(angle)), -0.25, 0.25);
        ((1.0 - celestial * rain_factor * thunder_factor) * 11.0) as i32
    }

    /// `Level.isDay()`: the sky is still bright enough that a bed is refused.
    ///
    /// Vanilla also requires `!dimensionType().hasFixedTime()`, so a fixed-time
    /// dimension is never "day" and never refuses a bed; that gate belongs to
    /// the caller, which is the only place the dimension type is known.
    #[must_use]
    pub fn is_day(day_time: i64, rain_level: f32, thunder_level: f32) -> bool {
        Self::sky_darken(day_time, rain_level, thunder_level) < SKY_DARKEN_DAY_THRESHOLD
    }

    /// `ServerPlayer.startSleepInBed`'s night gate, from the ramped levels:
    /// `!Level.isDay()`.
    ///
    /// This is the exact form. [`is_sleep_time`](Self::is_sleep_time) is the
    /// boolean-flag form most call sites have; the tests prove the two agree.
    #[must_use]
    pub fn is_sleep_time_with_levels(day_time: i64, rain_level: f32, thunder_level: f32) -> bool {
        !Self::is_day(day_time, rain_level, thunder_level)
    }

    /// The audited sleep window: `12542..=23459` clear, `12010..=23991` raining,
    /// the whole day while thundering.
    ///
    /// `day_time` is any level day time; it is wrapped into `0..24000` with
    /// `rem_euclid` (the same wrap `Mth.frac` performs), so a negative or
    /// already-advanced day time answers correctly instead of falling outside
    /// both windows.
    ///
    /// The windows are the steady state of [`sky_darken`](Self::sky_darken) at
    /// full levels: rain widens the window because it darkens the sky, and a
    /// thunderstorm (`/weather thunder` sets rain and thunder together) darkens
    /// it past the threshold at every hour. A caller that knows the real ramp
    /// levels should use
    /// [`is_sleep_time_with_levels`](Self::is_sleep_time_with_levels) instead:
    /// thunder that outlives its rain is *not* always sleepable.
    #[must_use]
    pub fn is_sleep_time(day_time: i64, is_raining: bool, is_thundering: bool) -> bool {
        if is_thundering {
            return true;
        }
        let ticks = day_time.rem_euclid(DAY_LENGTH);
        let window = if is_raining {
            SLEEP_TIME_RAIN
        } else {
            SLEEP_TIME_CLEAR
        };
        (window.0..=window.1).contains(&ticks)
    }

    /// The `Monster` query box around a bed, the check
    /// `ServerPlayer.startSleepInBed` runs before it lets a player sleep.
    #[must_use]
    pub fn monster_check_box(bed_x: i32, bed_y: i32, bed_z: i32) -> MonsterCheckBox {
        MonsterCheckBox::around_bed(bed_x, bed_y, bed_z)
    }

    /// `SleepStatus.sleepersNeeded`: `Math.max(1, Mth.ceil(active * pct / 100f))`.
    ///
    /// The percentage is applied to the *total* player count of the dimension,
    /// not to those awake, and the ceiling means three players at `50` need two
    /// sleepers. `active * pct` is computed in `i64` here so a hand-edited
    /// `level.dat` cannot overflow the multiplication before the `f32` cast.
    #[must_use]
    pub fn sleepers_needed(total_in_dimension: u32, players_sleeping_percentage: i32) -> u32 {
        let product = i64::from(total_in_dimension) * i64::from(players_sleeping_percentage);
        let needed = (product as f32 / 100.0).ceil();
        // `f32 as u32` saturates, so an absurd percentage reports "everyone and
        // then some" rather than wrapping to zero.
        needed.max(1.0) as u32
    }

    /// `ServerLevel.canSleepThroughNights()`: `playersSleepingPercentage <= 100`.
    /// Above `100` the night is never skipped, however many players sleep.
    #[must_use]
    pub const fn can_sleep_through_nights(players_sleeping_percentage: i32) -> bool {
        players_sleeping_percentage <= 100
    }

    /// `SleepStatus.areEnoughSleeping`: enough players are in bed to skip the
    /// night.
    #[must_use]
    pub fn enough_players_sleeping(
        total_in_dimension: u32,
        sleeping_count: u32,
        players_sleeping_percentage: i32,
    ) -> bool {
        sleeping_count >= Self::sleepers_needed(total_in_dimension, players_sleeping_percentage)
    }

    /// `ServerLevel`'s sleep skip: advance to the next multiple of `24000`.
    ///
    /// `long nextDay = dayTime + 24000L; setDayTime(nextDay - nextDay % 24000L)`
    /// — the truncated remainder Java uses, so the answer is the first day
    /// boundary strictly after `current_day_time` unless that time already is
    /// one, in which case it is the next day's.
    #[must_use]
    pub fn sleep_skip_target_time(current_day_time: i64) -> i64 {
        let next_day = current_day_time.saturating_add(DAY_LENGTH);
        next_day - next_day % DAY_LENGTH
    }
}

// ---------- phantom insomnia ----------

/// `PhantomSpawner`: the `Stats.TIME_SINCE_REST` threshold, in ticks.
pub const INSOMNIA_THRESHOLD_TICKS: u64 = 72_000;

/// `PhantomSpawner`: `if (level.getSkyDarken() < 5 && hasSkyLight) return 0`.
pub const PHANTOM_MIN_SKY_DARKEN: i32 = 5;

/// `PhantomSpawner.nextTick`: `(60 + nextInt(60)) * 20`, i.e. `1200..2400`
/// ticks (60-120 seconds) as a half-open span.
pub const PHANTOM_SPAWN_COOLDOWN: (u32, u32) = (1_200, 2_400);

/// `PhantomSpawner`: the difficulty gate is
/// `difficulty.isHarderThan(nextFloat() * 3.0F)`.
pub const PHANTOM_DIFFICULTY_ROLL_FACTOR: f32 = 3.0;

/// The Vanilla 1.21.1 phantom-insomnia rules.
pub struct PhantomInsomnia;

impl PhantomInsomnia {
    /// `PhantomSpawner`'s per-player insomnia gate:
    /// `random.nextInt(clampedTicksSinceRest) >= 72000`.
    ///
    /// `roll_in_ticks` must be a draw in `0..ticks_since_rest` — that is what
    /// makes the two clauses the same condition, since a draw below `72001`
    /// cannot reach `72000`. Passing a roll generated against a different bound
    /// is the caller's bug; the explicit `> 72000` term keeps this function
    /// honest in that case instead of spawning phantoms after one night.
    ///
    /// The result is a probability, not a schedule: wall-clock resets and
    /// phantoms are the caller's to place (`doInsomnia`, sky light, sea level,
    /// `canSeeSky` and the difficulty roll all gate it as well).
    #[must_use]
    pub const fn should_spawn_phantom(ticks_since_rest: u64, roll_in_ticks: u64) -> bool {
        ticks_since_rest > INSOMNIA_THRESHOLD_TICKS && roll_in_ticks >= INSOMNIA_THRESHOLD_TICKS
    }

    /// `PhantomSpawner`'s spawn cooldown from a `nextInt(60)` draw:
    /// `(60 + draw) * 20` ticks.
    #[must_use]
    pub const fn spawn_cooldown_ticks(draw: u32) -> u32 {
        (60 + draw % 60) * 20
    }

    /// The `isHarderThan(nextFloat() * 3.0F)` gate, with the roll clamped into
    /// `[0, 1)` so a broken generator cannot fabricate a phantom.
    #[must_use]
    pub fn difficulty_gate_passes(local_difficulty: f32, roll: f32) -> bool {
        NaturalLightningPolicy::difficulty_is_harder_than(
            local_difficulty,
            Mth::clamp(roll, 0.0, 1.0) * PHANTOM_DIFFICULTY_ROLL_FACTOR,
        )
    }

    /// The pre-condition `PhantomSpawner` checks before walking the player list:
    /// the spawn attempt only happens when enemies may spawn, the `doInsomnia`
    /// gamerule is on, and the sky is dark (or the dimension has no sky light).
    #[must_use]
    pub const fn spawn_attempt_allowed(
        spawn_enemies: bool,
        do_insomnia: bool,
        sky_darken: i32,
        has_sky_light: bool,
    ) -> bool {
        spawn_enemies && do_insomnia && (!has_sky_light || sky_darken >= PHANTOM_MIN_SKY_DARKEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sampler that always returns `value`, so a timer lands on a known tick.
    fn fixed(value: i32) -> impl FnMut(i32) -> i32 {
        move |_| value
    }

    fn tick_once(state: &mut VanillaWeatherState) -> WeatherTickEvents {
        state.tick(true, true, &mut fixed(0))
    }

    #[test]
    fn weather_spans_match_the_vanilla_constants() {
        assert_eq!(RAIN_DELAY, (12_000, 180_000));
        assert_eq!(RAIN_DURATION, (12_000, 24_000));
        assert_eq!(THUNDER_DELAY, (12_000, 180_000));
        assert_eq!(THUNDER_DURATION, (3_600, 15_600));
        assert_eq!(RAIN_LEVEL_CHANGE_PER_TICK, 0.01);
        assert_eq!(THUNDER_LEVEL_CHANGE_PER_TICK, 0.01);
        assert_eq!(RAIN_LEVEL_VISIBLE_THRESHOLD, 0.2);
        assert_eq!(THUNDER_LEVEL_VISIBLE_THRESHOLD, 0.9);
    }

    #[test]
    fn game_event_ids_match_clientbound_game_event_packet() {
        assert_eq!(WeatherGameEvent::StartRaining.id(), 1);
        assert_eq!(WeatherGameEvent::StopRaining.id(), 2);
        assert_eq!(WeatherGameEvent::RainLevelChange(0.5).id(), 7);
        assert_eq!(WeatherGameEvent::ThunderLevelChange(0.5).id(), 8);
        assert_eq!(WeatherGameEvent::StartRaining.level(), None);
        assert_eq!(WeatherGameEvent::StopRaining.level(), None);
        assert_eq!(WeatherGameEvent::RainLevelChange(0.25).level(), Some(0.25));
        assert_eq!(WeatherGameEvent::ThunderLevelChange(1.0).level(), Some(1.0));
    }

    #[test]
    fn a_fresh_level_holds_a_rain_delay_before_its_first_rain() {
        let mut state = VanillaWeatherState::new();
        let events = tick_once(&mut state);
        // A zero timer with the flag clear samples the delay, it does not rain.
        assert_eq!(state.rain_time(), RAIN_DELAY.0);
        assert_eq!(state.thunder_time(), THUNDER_DELAY.0);
        assert!(!state.is_raining());
        assert!(!state.is_thundering());
        assert_eq!(state.rain_level(), 0.0);
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn rain_timer_expiry_toggles_rain_then_resamples_on_the_next_tick() {
        let mut state = VanillaWeatherState::from_saved(0, 3, 900, false, false);
        assert!(tick_once(&mut state).is_empty());
        assert!(tick_once(&mut state).is_empty());
        let events = tick_once(&mut state);

        // The timer reached zero: the flag flipped, and the duration is sampled
        // on the *next* tick, exactly as vanilla's `if (--rainTime == 0)` block
        // leaves the counter at zero for one tick.
        assert!(state.is_raining());
        assert_eq!(state.rain_time(), 0);
        // The thunder timer never paused: 900 -> 897.
        assert_eq!(state.thunder_time(), 897);
        assert_eq!(
            events.as_slice(),
            &[WeatherGameEvent::RainLevelChange(0.01)]
        );
        // START_RAINING waits for the ramp to cross 0.2.
        assert!(!events.contains_id(GAME_EVENT_START_RAINING));

        tick_once(&mut state);
        assert_eq!(state.rain_time(), RAIN_DURATION.0);
        assert_eq!(state.rain_level(), 0.02);
    }

    #[test]
    fn the_ramp_crosses_twenty_percent_on_the_twentieth_tick() {
        let mut state = VanillaWeatherState::from_saved(0, 1, 900, false, false);
        let mut crossed = None;
        for step in 1..=40 {
            let previous = state.rain_level();
            let events = tick_once(&mut state);
            if events.contains_id(GAME_EVENT_START_RAINING) {
                crossed = Some((step, previous));
                break;
            }
        }
        // Twenty additions of 0.01F land one ulp above 0.2F, so the transition
        // fires on the twentieth tick and not the twenty-first. Accumulating in
        // f64 would put it on the twenty-first.
        let (step, previous) = crossed.expect("START_RAINING must fire");
        assert_eq!(step, 20);
        assert!(previous <= RAIN_LEVEL_VISIBLE_THRESHOLD);
        assert!(state.rain_level() > RAIN_LEVEL_VISIBLE_THRESHOLD);
        assert_eq!(state.rain_level(), previous + RAIN_LEVEL_CHANGE_PER_TICK);
        assert!(state.is_raining_visible());
    }

    #[test]
    fn each_ramp_tick_broadcasts_a_rain_level_change() {
        let mut state = VanillaWeatherState::from_saved(0, 0, 900, true, false);
        let first = tick_once(&mut state);
        assert_eq!(first.as_slice(), &[WeatherGameEvent::RainLevelChange(0.01)]);
        assert!(first.contains_id(GAME_EVENT_RAIN_LEVEL_CHANGE));

        let second = tick_once(&mut state);
        assert_eq!(second.len(), 1);
        assert_eq!(
            second.as_slice(),
            &[WeatherGameEvent::RainLevelChange(0.02)]
        );
        // The thunder level sits at 0.0 and stays there, so no game event 8.
        assert!(!second.contains_id(GAME_EVENT_THUNDER_LEVEL_CHANGE));
        assert_eq!(state.thunder_level(), 0.0);
    }

    #[test]
    fn the_rain_level_clamps_at_one_and_stops_emitting_once_settled() {
        let mut state = VanillaWeatherState::from_saved(0, 0, 900, true, false);
        let mut ticks_to_full = None;
        for step in 1..=200 {
            tick_once(&mut state);
            if state.rain_level() == 1.0 && ticks_to_full.is_none() {
                ticks_to_full = Some(step);
            }
        }
        // 0.01F accumulated 101 times reaches exactly 1.0 after clamping.
        assert_eq!(ticks_to_full, Some(101));
        assert_eq!(state.rain_level(), 1.0);
        assert!(state.is_raining_visible());

        let events = tick_once(&mut state);
        assert!(
            events.is_empty(),
            "a settled level broadcasts nothing: {events:?}"
        );
    }

    #[test]
    fn clearing_up_ramps_down_and_broadcasts_stop_raining_at_the_threshold() {
        let mut state = VanillaWeatherState::from_saved(0, 120, 900, true, false);
        for _ in 0..120 {
            tick_once(&mut state);
        }
        // The rain timer expired on the 120th tick and the ramp has started
        // coming back down.
        assert!(!state.is_raining());
        assert_eq!(state.rain_level(), 1.0 - RAIN_LEVEL_CHANGE_PER_TICK);

        let mut stop_step = None;
        let mut ramp_steps = 0;
        let mut previous = state.rain_level();
        for step in 1..=200 {
            let events = tick_once(&mut state);
            if events.contains_id(GAME_EVENT_RAIN_LEVEL_CHANGE) {
                ramp_steps += 1;
            }
            if events.contains_id(GAME_EVENT_STOP_RAINING) {
                assert!(stop_step.is_none(), "STOP_RAINING must fire once");
                assert!(previous > RAIN_LEVEL_VISIBLE_THRESHOLD);
                assert!(state.rain_level() <= RAIN_LEVEL_VISIBLE_THRESHOLD);
                stop_step = Some((step, ramp_steps));
            }
            previous = state.rain_level();
        }
        // 1.0 - 0.01F eighty-one times is the first level at or below 0.2F; the
        // first subtraction happened on the flip tick, so the threshold lands 80
        // continuation ticks later.
        assert_eq!(stop_step, Some((80, 80)));
        assert!(!state.is_raining_visible());
        assert_eq!(state.rain_level(), 0.0);
    }

    #[test]
    fn the_thunder_timer_runs_independently_of_the_rain_timer() {
        // Raining, a 2-tick rain timeout and a 10-tick thunder timeout: rain
        // stops first and thunder keeps counting on its own clock.
        let mut state = VanillaWeatherState::from_saved(0, 2, 10, true, true);
        for _ in 0..3 {
            tick_once(&mut state);
        }
        assert!(!state.is_raining(), "the rain timer toggled rain off");
        assert!(state.is_thundering(), "thunder is not tied to rain");
        assert_eq!(state.rain_time(), RAIN_DELAY.0);
        assert_eq!(state.thunder_time(), 7);

        for _ in 0..7 {
            tick_once(&mut state);
        }
        assert!(
            !state.is_thundering(),
            "the thunder timer expired on its own"
        );
        assert_eq!(state.thunder_time(), 0);
        // The delay is sampled the tick after the timer hits zero.
        tick_once(&mut state);
        assert_eq!(state.thunder_time(), THUNDER_DELAY.0);
    }

    #[test]
    fn thunder_can_start_while_it_is_not_raining() {
        let mut state = VanillaWeatherState::from_saved(0, 900, 1, false, false);
        let events = tick_once(&mut state);
        assert!(state.is_thundering());
        assert!(!state.is_raining());
        assert_eq!(state.thunder_time(), 0);
        assert_eq!(state.rain_time(), 899);
        assert_eq!(
            events.as_slice(),
            &[WeatherGameEvent::ThunderLevelChange(0.01)]
        );

        tick_once(&mut state);
        assert_eq!(state.thunder_time(), THUNDER_DURATION.0);
    }

    #[test]
    fn thunder_level_changes_broadcast_game_event_eight() {
        let mut state = VanillaWeatherState::from_saved(0, 900, 1, true, false);
        let events = tick_once(&mut state);
        assert!(
            events.contains_id(GAME_EVENT_THUNDER_LEVEL_CHANGE),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .find(|event| event.id() == GAME_EVENT_THUNDER_LEVEL_CHANGE)
                .copied(),
            Some(WeatherGameEvent::ThunderLevelChange(0.01))
        );
        // Rain is broadcast first, then thunder, matching the bytecode order.
        assert_eq!(events.len(), 2);
        assert_eq!(
            events.as_slice()[0],
            WeatherGameEvent::RainLevelChange(0.01)
        );
        assert_eq!(
            events.as_slice()[1],
            WeatherGameEvent::ThunderLevelChange(0.01)
        );
    }

    #[test]
    fn weather_cycle_gamerule_freezes_the_timers_but_not_the_ramp() {
        let mut state = VanillaWeatherState::from_saved(0, 5, 7, true, false);
        let events = state.tick(false, true, &mut fixed(0));
        assert_eq!(state.rain_time(), 5);
        assert_eq!(state.thunder_time(), 7);
        assert_eq!(state.rain_level(), 0.01);
        assert!(events.contains_id(GAME_EVENT_RAIN_LEVEL_CHANGE));
    }

    #[test]
    fn a_dimension_without_sky_light_advances_nothing() {
        let mut state = VanillaWeatherState::from_saved(0, 5, 7, true, true);
        let events = state.tick(true, false, &mut fixed(0));
        assert_eq!(state.rain_time(), 5);
        assert_eq!(state.thunder_time(), 7);
        assert_eq!(state.rain_level(), 0.0);
        assert_eq!(state.thunder_level(), 0.0);
        assert!(events.is_empty());
    }

    #[test]
    fn the_clear_weather_time_branch_remembers_what_was_falling() {
        // A reloaded level with a forced clear window already running: the timers
        // are pinned to 0 while the stored flags still say "raining".
        let mut state = VanillaWeatherState::from_saved(3, 500, 500, true, true);
        state.tick(true, true, &mut fixed(0));
        assert_eq!(state.clear_weather_time(), 2);
        assert_eq!(state.rain_time(), 0);
        assert_eq!(state.thunder_time(), 0);
        assert!(!state.is_raining());
        assert!(!state.is_thundering());

        // From the second tick on, the flags are clear, so the branch pins the
        // timers to 1 instead.
        state.tick(true, true, &mut fixed(0));
        assert_eq!(state.clear_weather_time(), 1);
        assert_eq!(state.rain_time(), 1);
        assert_eq!(state.thunder_time(), 1);

        state.tick(true, true, &mut fixed(0));
        assert_eq!(state.clear_weather_time(), 0);
        // The branch still ran this tick, so nothing has resolved yet.
        assert!(!state.is_raining());

        // Outside the branch, the pinned timer expires immediately and rain and
        // thunder resume together: `/weather clear` postpones precipitation, it
        // does not cancel it.
        state.tick(true, true, &mut fixed(0));
        assert!(state.is_raining());
        assert!(state.is_thundering());
        assert_eq!(state.rain_level(), 0.01);
        assert_eq!(state.thunder_level(), 0.01);
    }

    #[test]
    fn set_clear_leaves_rain_ready_to_resume_after_the_countdown() {
        let mut state = VanillaWeatherState::new();
        state.set_clear(2);
        state.tick(true, true, &mut fixed(0));
        assert_eq!(state.clear_weather_time(), 1);
        assert_eq!(state.rain_time(), 1);
        assert_eq!(state.thunder_time(), 1);
        state.tick(true, true, &mut fixed(0));
        assert_eq!(state.clear_weather_time(), 0);
        state.tick(true, true, &mut fixed(0));
        assert!(state.is_raining());
        assert!(state.is_thundering());
        assert_eq!(state.rain_time(), 0);
        state.tick(true, true, &mut fixed(0));
        assert_eq!(state.rain_time(), RAIN_DURATION.0);
        assert_eq!(state.thunder_time(), THUNDER_DURATION.0);
    }

    #[test]
    fn weather_commands_set_the_fields_vanilla_sets() {
        let mut state = VanillaWeatherState::new();
        state.set_rain(1_000);
        assert_eq!(state.clear_weather_time(), 0);
        assert_eq!(state.rain_time(), 1_000);
        assert_eq!(state.thunder_time(), 1_000);
        assert!(state.is_raining());
        assert!(!state.is_thundering());

        state.set_thunder(2_000);
        assert_eq!(state.rain_time(), 2_000);
        assert_eq!(state.thunder_time(), 2_000);
        assert!(state.is_raining());
        assert!(state.is_thundering());

        state.set_clear(3_000);
        assert_eq!(state.clear_weather_time(), 3_000);
        assert_eq!(state.rain_time(), 0);
        assert_eq!(state.thunder_time(), 0);
        assert!(!state.is_raining());
        assert!(!state.is_thundering());

        // A negative duration cannot come through the command's argument type;
        // clamping keeps a hand-built call from inverting the timer.
        state.set_rain(-5);
        assert_eq!(state.rain_time(), 0);
        assert_eq!(state.thunder_time(), 0);
    }

    #[test]
    fn timer_samples_stay_inside_the_vanilla_spans() {
        for sample in [0, 1, 12_000, 179_999, 180_000, 1_000_000] {
            let mut state = VanillaWeatherState::from_saved(0, 0, 0, false, false);
            state.tick(true, true, &mut fixed(sample));
            assert!(
                (RAIN_DELAY.0..=RAIN_DELAY.1).contains(&state.rain_time()),
                "rain delay {} outside the span for sample {sample}",
                state.rain_time()
            );
            assert!(
                (THUNDER_DELAY.0..=THUNDER_DELAY.1).contains(&state.thunder_time()),
                "thunder delay {} outside the span for sample {sample}",
                state.thunder_time()
            );
        }

        let mut state = VanillaWeatherState::from_saved(0, 0, 0, true, true);
        state.tick(true, true, &mut fixed(0));
        assert_eq!(state.rain_time(), RAIN_DURATION.0);
        assert_eq!(state.thunder_time(), THUNDER_DURATION.0);

        let mut state = VanillaWeatherState::from_saved(0, 0, 0, true, true);
        state.tick(true, true, &mut fixed(1_000_000));
        assert_eq!(state.rain_time(), RAIN_DURATION.1);
        assert_eq!(state.thunder_time(), THUNDER_DURATION.1);
    }

    #[test]
    fn a_hundred_thousand_tick_weather_run_is_deterministic() {
        let mut first = VanillaWeatherState::new();
        let mut second = VanillaWeatherState::new();
        let mut first_rng = crate::random::Xoroshiro128pp::new(0x5EED);
        let mut second_rng = crate::random::Xoroshiro128pp::new(0x5EED);
        for _ in 0..100_000 {
            let left = first.tick(true, true, &mut first_rng);
            let right = second.tick(true, true, &mut second_rng);
            assert_eq!(left, right);
            assert_eq!(first, second);
        }
        assert!((0.0..=1.0).contains(&first.rain_level()));
        assert!((0.0..=1.0).contains(&first.thunder_level()));
    }

    #[test]
    fn lightning_needs_rain_thunder_and_a_zero_roll() {
        assert!(NaturalLightningPolicy::should_strike_chunk(true, true, 0));
        assert!(!NaturalLightningPolicy::should_strike_chunk(true, true, 1));
        assert!(!NaturalLightningPolicy::should_strike_chunk(
            true,
            true,
            LIGHTNING_CHUNK_ROLL_BOUND - 1
        ));
        assert!(!NaturalLightningPolicy::should_strike_chunk(false, true, 0));
        assert!(!NaturalLightningPolicy::should_strike_chunk(true, false, 0));
        assert_eq!(LIGHTNING_CHUNK_ROLL_BOUND, 100_000);
        assert_eq!(LIGHTNING_ROD_SEARCH_RADIUS, 128);
    }

    #[test]
    fn skeleton_trap_chance_scales_with_local_difficulty() {
        assert_eq!(NaturalLightningPolicy::skeleton_trap_horse_chance(0.0), 0.0);
        // `(double) difficulty * 0.01`: the decimal 0.01 is not representable, so
        // the products are pinned with a tolerance rather than an equality.
        for (difficulty, expected) in [(1.0, 0.01), (2.0, 0.02), (6.75, 0.0675)] {
            let chance = NaturalLightningPolicy::skeleton_trap_horse_chance(difficulty);
            assert!(
                (chance - expected).abs() < 1e-12,
                "{difficulty} -> {chance}, expected {expected}"
            );
        }
        assert!(NaturalLightningPolicy::should_spawn_skeleton_trap(
            true, 3.0, 0.029, false
        ));
        assert!(!NaturalLightningPolicy::should_spawn_skeleton_trap(
            true, 3.0, 0.031, false
        ));
        assert!(!NaturalLightningPolicy::should_spawn_skeleton_trap(
            true, 3.0, 0.0, true
        ));
        assert!(!NaturalLightningPolicy::should_spawn_skeleton_trap(
            false, 3.0, 0.0, false
        ));
        // A broken generator cannot fabricate a trap.
        assert!(!NaturalLightningPolicy::should_spawn_skeleton_trap(
            true, 0.0, 5.0, false
        ));
        assert!(NaturalLightningPolicy::difficulty_is_harder_than(
            6.75, 6.74
        ));
        assert!(!NaturalLightningPolicy::difficulty_is_harder_than(0.0, 0.0));
    }

    #[test]
    fn the_sleep_windows_are_the_audited_boundaries() {
        assert_eq!(SLEEP_TIME_CLEAR, (12_542, 23_459));
        assert_eq!(SLEEP_TIME_RAIN, (12_010, 23_991));

        assert!(!VanillaSleepState::is_sleep_time(12_541, false, false));
        assert!(VanillaSleepState::is_sleep_time(12_542, false, false));
        assert!(VanillaSleepState::is_sleep_time(23_459, false, false));
        assert!(!VanillaSleepState::is_sleep_time(23_460, false, false));

        assert!(!VanillaSleepState::is_sleep_time(12_009, true, false));
        assert!(VanillaSleepState::is_sleep_time(12_010, true, false));
        assert!(VanillaSleepState::is_sleep_time(23_991, true, false));
        assert!(!VanillaSleepState::is_sleep_time(23_992, true, false));

        // Thundering is sleepable at every hour.
        for tick in [0, 1_000, 6_000, 12_000, 18_000, 23_999] {
            assert!(
                VanillaSleepState::is_sleep_time(tick, false, true),
                "{tick}"
            );
            assert!(VanillaSleepState::is_sleep_time(tick, true, true), "{tick}");
        }
        // Daylight is not.
        for tick in [0, 1_000, 6_000, 12_000, 12_541] {
            assert!(
                !VanillaSleepState::is_sleep_time(tick, false, false),
                "{tick}"
            );
        }
    }

    #[test]
    fn sleep_time_wraps_the_day_time_instead_of_falling_outside() {
        assert_eq!(
            VanillaSleepState::is_sleep_time(12_542 + DAY_LENGTH * 3, false, false),
            VanillaSleepState::is_sleep_time(12_542, false, false)
        );
        assert_eq!(
            VanillaSleepState::is_sleep_time(-DAY_LENGTH + 12_542, false, false),
            VanillaSleepState::is_sleep_time(12_542, false, false)
        );
        assert_eq!(
            VanillaSleepState::is_sleep_time(-1, false, false),
            VanillaSleepState::is_sleep_time(23_999, false, false)
        );
        let residue = i64::MIN.rem_euclid(DAY_LENGTH);
        assert_eq!(
            VanillaSleepState::is_sleep_time(i64::MIN, false, false),
            (SLEEP_TIME_CLEAR.0..=SLEEP_TIME_CLEAR.1).contains(&residue)
        );
    }

    #[test]
    fn sky_darken_matches_the_vanilla_formula_at_pinned_times() {
        // Sunrise, noon, sunset, midnight in clear weather.
        assert_eq!(VanillaSleepState::sky_darken(0, 0.0, 0.0), 0);
        assert_eq!(VanillaSleepState::sky_darken(6_000, 0.0, 0.0), 0);
        assert_eq!(VanillaSleepState::sky_darken(12_000, 0.0, 0.0), 0);
        assert_eq!(VanillaSleepState::sky_darken(18_000, 0.0, 0.0), 11);
        // The two threshold edges the clear-weaather window is derived from.
        assert_eq!(VanillaSleepState::sky_darken(12_541, 0.0, 0.0), 3);
        assert_eq!(VanillaSleepState::sky_darken(12_542, 0.0, 0.0), 4);
        assert_eq!(VanillaSleepState::sky_darken(23_459, 0.0, 0.0), 4);
        assert_eq!(VanillaSleepState::sky_darken(23_460, 0.0, 0.0), 3);
        // Rain darkens the sky, so the window widens on both ends.
        assert_eq!(VanillaSleepState::sky_darken(12_009, 1.0, 0.0), 3);
        assert_eq!(VanillaSleepState::sky_darken(12_010, 1.0, 0.0), 4);
        assert_eq!(VanillaSleepState::sky_darken(23_991, 1.0, 0.0), 4);
        assert_eq!(VanillaSleepState::sky_darken(23_992, 1.0, 0.0), 3);
        // Rain plus thunder is night all day...
        assert_eq!(VanillaSleepState::sky_darken(6_000, 1.0, 1.0), 5);
        assert!(VanillaSleepState::is_sleep_time_with_levels(
            6_000, 1.0, 1.0
        ));
        // ...but thunder without rain does not reach the threshold at noon, which
        // is why the boolean form documents itself as the steady state.
        assert_eq!(VanillaSleepState::sky_darken(6_000, 0.0, 1.0), 3);
        assert!(!VanillaSleepState::is_sleep_time_with_levels(
            6_000, 0.0, 1.0
        ));
    }

    #[test]
    fn the_window_table_equals_the_sky_darken_derivation_every_tick() {
        for tick in 0..DAY_LENGTH {
            assert_eq!(
                VanillaSleepState::is_sleep_time(tick, false, false),
                VanillaSleepState::is_sleep_time_with_levels(tick, 0.0, 0.0),
                "clear window disagrees at {tick}"
            );
            assert_eq!(
                VanillaSleepState::is_sleep_time(tick, true, false),
                VanillaSleepState::is_sleep_time_with_levels(tick, 1.0, 0.0),
                "rain window disagrees at {tick}"
            );
            assert_eq!(
                VanillaSleepState::is_sleep_time(tick, true, true),
                VanillaSleepState::is_sleep_time_with_levels(tick, 1.0, 1.0),
                "thunder window disagrees at {tick}"
            );
        }
    }

    #[test]
    fn is_day_is_the_complement_of_the_sleep_derivation() {
        assert!(VanillaSleepState::is_day(6_000, 0.0, 0.0));
        assert!(!VanillaSleepState::is_day(18_000, 0.0, 0.0));
        assert!(!VanillaSleepState::is_day(12_542, 0.0, 0.0));
        for tick in [0, 6_000, 12_541, 12_542, 23_459, 23_460] {
            assert_eq!(
                VanillaSleepState::is_day(tick, 0.0, 0.0),
                !VanillaSleepState::is_sleep_time_with_levels(tick, 0.0, 0.0)
            );
        }
        // Day time is wrapped the way `Mth.frac` wraps it.
        assert_eq!(
            VanillaSleepState::sky_darken(18_000 + DAY_LENGTH, 0.0, 0.0),
            VanillaSleepState::sky_darken(18_000, 0.0, 0.0)
        );
    }

    #[test]
    fn monster_check_box_is_the_eight_by_five_by_eight_bed_query() {
        let bounds = MonsterCheckBox::around_bed(10, 64, -10);
        assert_eq!(bounds.min, [2.5, 59.0, -17.5]);
        assert_eq!(bounds.max, [18.5, 69.0, -1.5]);
        assert_eq!(bounds.max[0] - bounds.min[0], 16.0);
        assert_eq!(bounds.max[1] - bounds.min[1], 10.0);
        assert_eq!(bounds.max[2] - bounds.min[2], 16.0);
        assert_eq!(MONSTER_CHECK_RADIUS_XZ, 8.0);
        assert_eq!(MONSTER_CHECK_RADIUS_Y, 5.0);

        // Half-open like `AABB.contains`: the minimum corner counts, the maximum
        // does not.
        assert!(bounds.contains_point(2.5, 59.0, -17.5));
        assert!(bounds.contains_point(10.5, 64.0, -9.5));
        assert!(!bounds.contains_point(18.5, 64.0, -9.5));
        assert!(!bounds.contains_point(10.5, 69.0, -9.5));
        assert!(!bounds.contains_point(10.5, 64.0, -1.5));
        assert!(!bounds.contains_point(2.4, 64.0, -9.5));

        assert_eq!(
            VanillaSleepState::monster_check_box(10, 64, -10),
            MonsterCheckBox::around_bed(10, 64, -10)
        );

        // The entity-box test `getEntitiesOfClass` actually runs.
        let monster = MonsterCheckBox {
            min: [4.0, 60.0, -12.0],
            max: [5.0, 62.0, -11.0],
        };
        assert!(bounds.intersects(&monster));
        let far = MonsterCheckBox {
            min: [100.0, 60.0, -12.0],
            max: [101.0, 62.0, -11.0],
        };
        assert!(!bounds.intersects(&far));
        assert!(bounds.intersects(&bounds));
    }

    #[test]
    fn players_sleeping_percentage_matches_sleep_status() {
        // Zero means "one sleeper is enough", not "nobody has to sleep".
        assert_eq!(VanillaSleepState::sleepers_needed(10, 0), 1);
        assert!(VanillaSleepState::enough_players_sleeping(10, 1, 0));
        assert!(!VanillaSleepState::enough_players_sleeping(10, 0, 0));

        // 50% of three players is 1.5, which the ceiling rounds up to two.
        assert_eq!(VanillaSleepState::sleepers_needed(3, 50), 2);
        assert!(!VanillaSleepState::enough_players_sleeping(3, 1, 50));
        assert!(VanillaSleepState::enough_players_sleeping(3, 2, 50));
        assert_eq!(VanillaSleepState::sleepers_needed(2, 50), 1);
        assert_eq!(VanillaSleepState::sleepers_needed(20, 50), 10);

        // 100% needs everyone.
        assert_eq!(VanillaSleepState::sleepers_needed(10, 100), 10);
        assert!(!VanillaSleepState::enough_players_sleeping(10, 9, 100));
        assert!(VanillaSleepState::enough_players_sleeping(10, 10, 100));

        // Above 100 the night can never be skipped.
        for total in 1..=20 {
            assert!(
                !VanillaSleepState::enough_players_sleeping(total, total, 101),
                "{total} players at 101% must not skip the night"
            );
            assert!(VanillaSleepState::enough_players_sleeping(
                total, total, 100
            ));
        }
        assert_eq!(VanillaSleepState::sleepers_needed(0, 100), 1);
        assert!(!VanillaSleepState::enough_players_sleeping(0, 0, 100));

        assert!(VanillaSleepState::can_sleep_through_nights(100));
        assert!(VanillaSleepState::can_sleep_through_nights(0));
        assert!(VanillaSleepState::can_sleep_through_nights(-1));
        assert!(!VanillaSleepState::can_sleep_through_nights(101));
    }

    #[test]
    fn sleep_skip_advances_to_the_next_day_boundary() {
        assert_eq!(VanillaSleepState::sleep_skip_target_time(0), 24_000);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(1), 24_000);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(18_000), 24_000);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(23_999), 24_000);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(24_000), 48_000);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(24_001), 48_000);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(25_000), 48_000);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(47_999), 48_000);
        // Java's truncated remainder, which is what the bytecode uses.
        assert_eq!(VanillaSleepState::sleep_skip_target_time(-100), 0);
        assert_eq!(VanillaSleepState::sleep_skip_target_time(-24_000), 0);
        assert_eq!(
            VanillaSleepState::sleep_skip_target_time(i64::MAX),
            i64::MAX - i64::MAX % DAY_LENGTH
        );
        // Every answer is a multiple of 24000 at or after the input day.
        for day in [0, 1, 12_542, 23_999, 24_000, 250_000] {
            let target = VanillaSleepState::sleep_skip_target_time(day);
            assert_eq!(target % DAY_LENGTH, 0);
            assert!(target > day);
        }
    }

    #[test]
    fn phantom_insomnia_needs_three_days_without_rest_and_a_high_roll() {
        assert_eq!(INSOMNIA_THRESHOLD_TICKS, 72_000);
        assert!(!PhantomInsomnia::should_spawn_phantom(72_000, 71_999));
        // A draw of 72000 out of 72001 ticks is the one success.
        assert!(PhantomInsomnia::should_spawn_phantom(72_001, 72_000));
        assert!(!PhantomInsomnia::should_spawn_phantom(72_001, 71_999));
        assert!(PhantomInsomnia::should_spawn_phantom(200_000, 100_000));
        assert!(!PhantomInsomnia::should_spawn_phantom(200_000, 0));
        // A roll drawn against the wrong bound cannot pass on its own.
        assert!(!PhantomInsomnia::should_spawn_phantom(1, 999_999));
        assert!(!PhantomInsomnia::should_spawn_phantom(0, 0));

        // Exactly one of the 72001 possible draws at the first eligible tick.
        let eligible = (0..=72_000u64)
            .filter(|roll| PhantomInsomnia::should_spawn_phantom(72_001, *roll))
            .count();
        assert_eq!(eligible, 1);
    }

    #[test]
    fn phantom_cooldown_is_sixty_to_one_hundred_and_nineteen_seconds() {
        assert_eq!(PHANTOM_SPAWN_COOLDOWN, (1_200, 2_400));
        assert_eq!(PhantomInsomnia::spawn_cooldown_ticks(0), 1_200);
        assert_eq!(PhantomInsomnia::spawn_cooldown_ticks(1), 1_220);
        assert_eq!(PhantomInsomnia::spawn_cooldown_ticks(59), 2_380);
        // A draw outside the 60-value span wraps rather than escaping it.
        assert_eq!(PhantomInsomnia::spawn_cooldown_ticks(60), 1_200);
        for draw in 0..60 {
            let ticks = PhantomInsomnia::spawn_cooldown_ticks(draw);
            assert!(
                (PHANTOM_SPAWN_COOLDOWN.0..PHANTOM_SPAWN_COOLDOWN.1).contains(&ticks),
                "cooldown {ticks} outside the vanilla span"
            );
        }
    }

    #[test]
    fn phantom_difficulty_gate_and_sky_precondition() {
        // `isHarderThan(nextFloat() * 3.0F)`
        assert!(PhantomInsomnia::difficulty_gate_passes(2.0, 0.5));
        assert!(!PhantomInsomnia::difficulty_gate_passes(1.0, 0.5));
        assert!(PhantomInsomnia::difficulty_gate_passes(3.0, 0.999));
        assert!(!PhantomInsomnia::difficulty_gate_passes(0.0, 0.0));
        assert!(!PhantomInsomnia::difficulty_gate_passes(0.0, 5.0));

        assert_eq!(PHANTOM_MIN_SKY_DARKEN, 5);
        // `skyDarken < 5` in a sky-lit dimension refuses the attempt.
        assert!(!PhantomInsomnia::spawn_attempt_allowed(true, true, 4, true));
        assert!(PhantomInsomnia::spawn_attempt_allowed(true, true, 5, true));
        // A dimension with no sky light skips that check.
        assert!(PhantomInsomnia::spawn_attempt_allowed(true, true, 0, false));
        assert!(!PhantomInsomnia::spawn_attempt_allowed(
            false, true, 11, true
        ));
        assert!(!PhantomInsomnia::spawn_attempt_allowed(
            true, false, 11, true
        ));
    }

    #[test]
    fn tick_events_buffer_never_overflows() {
        let mut events = WeatherTickEvents::new();
        assert!(events.is_empty());
        assert_eq!(events.len(), 0);
        assert!(events.iter().next().is_none());
        events.push(WeatherGameEvent::RainLevelChange(0.5));
        events.push(WeatherGameEvent::ThunderLevelChange(0.5));
        events.push(WeatherGameEvent::StartRaining);
        assert_eq!(events.len(), MAX_EVENTS_PER_TICK);
        events.push(WeatherGameEvent::StopRaining);
        assert_eq!(events.len(), MAX_EVENTS_PER_TICK);
        assert_eq!(
            events.as_slice(),
            &[
                WeatherGameEvent::RainLevelChange(0.5),
                WeatherGameEvent::ThunderLevelChange(0.5),
                WeatherGameEvent::StartRaining,
            ]
        );
        assert_eq!(events.iter().count(), MAX_EVENTS_PER_TICK);
        assert_eq!((&events).into_iter().count(), MAX_EVENTS_PER_TICK);
        assert_eq!(WeatherTickEvents::default(), WeatherTickEvents::new());
        assert!(!WeatherTickEvents::new().contains_id(GAME_EVENT_START_RAINING));
    }
}
