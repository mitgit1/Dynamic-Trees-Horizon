// Dynamic Trees for Horizon 26.3 (v0.2.97) — native mixin runtime.
// Injected as `pub(crate) mod dynamictrees` into crates/horizon-client/src/game.rs.
// Every game API used here was verified against Horizon a3acb5f0d2f59d2d5ff21a5aa000809ba1e35e2f.

use super::{Game, Result};
use horizon_core::position::BlockPos;
use horizon_world::level::flags;
use horizon_world::registry::BlockStateId;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;

// Growth pacing, in game ticks (20 per second).
const GROW_DELAY: i64 = 60; // the sapling sits this long before the trunk starts
const TRUNK_INTERVAL: i64 = 10; // one trunk log every so many ticks
const CANOPY_DELAY: i64 = 10; // pause between the finished trunk and the canopy
const CANOPY_PER_TICK: usize = 8; // leaves placed per tick during the canopy
const SCAN_INTERVAL: i64 = 20; // the sapling scan period
const MAX_CLAIMS: usize = 32; // trees growing at once, per world

// Topple limits.
const TOPPLE_MAX_LOGS: usize = 96;
const TOPPLE_MAX_LEAVES: usize = 320;
const TOPPLE_LEAF_DEPTH: usize = 4; // BFS layers of leaves away from the trunk

const SCAN_HALF_XZ: i32 = 12;
const SCAN_UP: i32 = 20;
const SCAN_DOWN: i32 = 6;

// The two things the mod does, called from the HEAD hooks.
pub(super) fn grow_tick(game: &mut Game<'_>) {
    if game.remote.is_some() {
        return; // joining a server: the server owns the world
    }
    with_trees(game, |game, trees| growth_tick(game, trees));
}

pub(super) fn topple(game: &mut Game<'_>, pos: [i32; 3]) -> Option<Result<()>> {
    if game.remote.is_some() {
        return None; // joining a server: let the server's own break run
    }
    with_trees(game, |game, _trees| fell_tree(game, pos))
}

// ---------------------------------------------------------------------------
// Per-world state (keyed by the world's save folder, as Veinminer's is).
// ---------------------------------------------------------------------------

/// One tree on the way up.
struct Growing {
    /// The sapling's cell; also the trunk's bottom.
    pos: [i32; 3],
    /// The sapling's block name, e.g. `minecraft:oak_sapling`.
    sapling: String,
    /// The trunk block chosen for this tree: a branch block when the content
    /// mod is on, otherwise the vanilla log.
    trunk: BlockStateId,
    /// The canopy block (nether wart blocks take no leaf properties).
    leaf: Option<BlockStateId>,
    /// Trunk cells, bottom first: the sapling cell, then `height` above it.
    trunk_cells: Vec<[i32; 3]>,
    /// Canopy cells with the leaf's `distance` property value.
    leaf_cells: Vec<([i32; 3], u8)>,
    /// How much of `trunk_cells` has been placed.
    trunk_done: usize,
    /// How much of `leaf_cells` has been placed.
    leaf_done: usize,
    /// 0 = waiting, 1 = raising the trunk, 2 = filling the canopy.
    phase: u8,
    next_step: i64,
}

#[derive(Default)]
struct Trees {
    last_scan: i64,
    growing: BTreeMap<[i32; 3], Growing>,
}

thread_local! {
    static STATES: RefCell<BTreeMap<PathBuf, Trees>> = RefCell::new(BTreeMap::new());
}

fn with_trees<T>(game: &mut Game<'_>, run: impl FnOnce(&mut Game<'_>, &mut Trees) -> T) -> T {
    struct Lease {
        key: PathBuf,
        state: Trees,
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            STATES.with(|states| {
                states
                    .borrow_mut()
                    .insert(self.key.clone(), std::mem::take(&mut self.state));
            });
        }
    }
    let key = game.world_root.clone();
    let state = STATES
        .with(|states| states.borrow_mut().remove(&key))
        .unwrap_or_default();
    let mut lease = Lease { key, state };
    run(game, &mut lease.state)
}

// ---------------------------------------------------------------------------
// Small read/write helpers over the game's own APIs.
// ---------------------------------------------------------------------------

fn name_of(game: &Game<'_>, state: BlockStateId) -> String {
    game.res.facts().get(state).name.to_string()
}

fn block_at(game: &Game<'_>, pos: [i32; 3]) -> BlockStateId {
    game.world.block(pos[0], pos[1], pos[2])
}

fn is_air_at(game: &Game<'_>, pos: [i32; 3]) -> bool {
    game.world.is_air(block_at(game, pos))
}

fn in_tag(game: &Game<'_>, tag: &str, block: &str) -> bool {
    game.res.items().block_in_tag(tag, block)
}

fn default_state(game: &Game<'_>, name: &str) -> Option<BlockStateId> {
    game.pack.assets.registry.default_state(name)
}

fn air_state(game: &Game<'_>) -> BlockStateId {
    default_state(game, "minecraft:air").unwrap_or(BlockStateId(0))
}

/// The state with `props` changed, through the game's own property resolver.
fn with_props(game: &Game<'_>, state: BlockStateId, props: &[(&str, &str)]) -> Option<BlockStateId> {
    game.with_props(state, props)
}

/// Set one block through the vanilla behaviour entry point.
/// `neighbors` controls the `UPDATE_NEIGHBORS` flag.
fn set_block(game: &mut Game<'_>, pos: [i32; 3], state: BlockStateId, neighbors: bool) {
    let at = BlockPos::new(pos[0], pos[1], pos[2]);
    let flag_set = flags::UPDATE_CLIENTS | if neighbors { flags::UPDATE_NEIGHBORS } else { 0 };
    game.with_level(|b, level, ticks| {
        let _ = b.set_block(level, ticks, at, state, flag_set);
    });
}

/// A cheap deterministic per-tree random: a hash of position, time and salt.
fn tree_rand(pos: [i32; 3], tick: i64, salt: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for word in [pos[0] as i64, pos[1] as i64, pos[2] as i64, tick, salt as i64] {
        for byte in word.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

// ---------------------------------------------------------------------------
// Species tables.
// ---------------------------------------------------------------------------

/// `mega_jungle` and friends grow the tree of their base species.
fn species_base(block: &str) -> String {
    let path = block.rsplit_once(':').map(|(_, p)| p).unwrap_or(block);
    let base = path.strip_suffix("_sapling").unwrap_or(path);
    match base {
        "mega_jungle" => "jungle".into(),
        "mega_spruce" => "spruce".into(),
        "mega_crimson" => "crimson".into(),
        "mega_warped" => "warped".into(),
        other => other.into(),
    }
}

/// Trunk block candidates, best first: the content mod's branch blocks when
/// installed, then vanilla logs (and nether stems).
fn trunk_candidates(game: &Game<'_>, sapling: &str) -> Option<BlockStateId> {
    let ns = sapling.split_once(':').map(|(n, _)| n).unwrap_or("minecraft");
    let base = species_base(sapling);
    let (wood, stem) = match base.as_str() {
        "azalea" | "flowering_azalea" => ("oak", None),
        _ => (base.as_str(), Some(base.clone())),
    };
    let names = [
        format!("{ns}:{base}_branch"),
        format!("dynamictrees:{base}_branch"),
        format!("minecraft:{wood}_log"),
    ]
    .into_iter()
    .chain(stem.map(|s| format!("minecraft:{s}_stem")))
    .chain(std::iter::once("minecraft:oak_log".to_owned()));
    names.filter_map(|n| default_state(game, &n)).next()
}

/// The canopy block, and whether it takes the leaf `distance`/`persistent` props.
fn canopy_of(game: &Game<'_>, sapling: &str) -> (Option<BlockStateId>, bool) {
    let base = species_base(sapling);
    let (name, props) = match base.as_str() {
        "crimson" | "warped" => ("minecraft:nether_wart_block".to_owned(), false),
        "azalea" => ("minecraft:azalea_leaves".to_owned(), true),
        "flowering_azalea" => ("minecraft:flowering_azalea_leaves".to_owned(), true),
        other => (format!("minecraft:{other}_leaves"), true),
    };
    (default_state(game, &name), props)
}

/// Trunk height per species, (min, max) inclusive.
fn trunk_height(sapling: &str) -> (i32, i32) {
    match species_base(sapling).as_str() {
        "spruce" | "pale_oak" | "jungle" => (6, 9),
        "crimson" | "warped" => (4, 8),
        _ => (4, 6),
    }
}

/// Whether the sapling's block takes the vanilla `stage` property at all.
fn has_stage(game: &Game<'_>, state: BlockStateId) -> bool {
    with_props(game, state, &[("stage", "1")]).is_some()
}

// ---------------------------------------------------------------------------
// Growth: claim saplings, raise the trunk, fill the canopy.
// ---------------------------------------------------------------------------

fn growth_tick(game: &mut Game<'_>, trees: &mut Trees) {
    let tick = game.world.game_time;

    // 1. Advance every claimed tree. Each is taken out of the map for its
    //    tick and put back unless it finished or died.
    let claims: Vec<[i32; 3]> = trees.growing.keys().copied().collect();
    for key in claims {
        if let Some(mut tree) = trees.growing.remove(&key)
            && step_tree(game, &mut tree, tick)
        {
            trees.growing.insert(key, tree);
        }
    }

    // 2. Look for new saplings now and then.
    if tick - trees.last_scan >= SCAN_INTERVAL {
        trees.last_scan = tick;
        scan_saplings(game, trees, tick);
    }
}

/// One claimed tree's tick. `false`: the claim is finished and dropped.
fn step_tree(game: &mut Game<'_>, tree: &mut Growing, tick: i64) -> bool {
    match tree.phase {
        // Waiting: keep the sapling at stage 0 so vanilla never grows it.
        0 => {
            if !sapling_still_there(game, tree) {
                cleanup(game, tree);
                return false;
            }
            hold_stage_zero(game, tree);
            if tick >= tree.next_step {
                tree.phase = 1;
                tree.next_step = tick;
            }
            true
        }
        // Trunk: one log per step. The first log replaces the sapling.
        1 => {
            if tree.trunk_done == 0 {
                if !sapling_still_there(game, tree) {
                    cleanup(game, tree);
                    return false;
                }
                hold_stage_zero(game, tree);
            }
            if tick >= tree.next_step {
                place_next_trunk_log(game, tree);
                tree.trunk_done += 1;
                tree.next_step = tick + TRUNK_INTERVAL;
                if tree.trunk_done >= tree.trunk_cells.len() {
                    tree.phase = 2;
                    tree.next_step = tick + CANOPY_DELAY;
                }
            }
            true
        }
        // Canopy: a few leaves per tick until the shape is done.
        2 => {
            if tick >= tree.next_step {
                fill_canopy(game, tree, CANOPY_PER_TICK);
                tree.next_step = tick;
                tree.leaf_done < tree.leaf_cells.len()
            }
            else {
                true
            }
        }
        _ => false,
    }
}

fn sapling_still_there(game: &Game<'_>, tree: &Growing) -> bool {
    name_of(game, block_at(game, tree.pos)) == tree.sapling
}

/// Reset a sapling back to stage 0: the same property change vanilla's own
/// `advanceTree` makes, so nothing needs redrawing.
fn hold_stage_zero(game: &mut Game<'_>, tree: &Growing) {
    let state = block_at(game, tree.pos);
    if let Some(reset) = with_props(game, state, &[("stage", "0")]) {
        if reset != state {
            set_block(game, tree.pos, reset, false);
        }
    }
}

fn place_next_trunk_log(game: &mut Game<'_>, tree: &mut Growing) {
    let cell = tree.trunk_cells[tree.trunk_done];
    if tree.trunk_done == 0 || is_air_at(game, cell) {
        set_block(game, cell, tree.trunk, true);
    }
}

fn fill_canopy(game: &mut Game<'_>, tree: &mut Growing, budget: usize) {
    let leaf = tree.leaf;
    let mut placed = 0;
    while tree.leaf_done < tree.leaf_cells.len() && placed < budget {
        let (cell, distance) = tree.leaf_cells[tree.leaf_done];
        tree.leaf_done += 1;
        if !is_air_at(game, cell) {
            continue;
        }
        let distance = distance.to_string();
        let state = match leaf {
            Some(base) => match with_props(
                game,
                base,
                &[("distance", distance.as_str()), ("persistent", "false")],
            ) {
                Some(state) => state,
                None => base, // the block takes no such properties (wart blocks)
            },
            None => continue,
        };
        set_block(game, cell, state, true);
        placed += 1;
    }
}

/// The staged trunk vanishes when its sapling is destroyed before the trunk
/// finished: a chopped sapling kills the young tree, and a vanilla growth
/// that got there first (bone meal) leaves no leftovers behind.
fn cleanup(game: &mut Game<'_>, tree: &Growing) {
    let trunk = tree.trunk;
    let air = air_state(game);
    for cell in tree.trunk_cells[..tree.trunk_done].iter() {
        if block_at(game, *cell) == trunk {
            set_block(game, *cell, air, true);
        }
    }
}

/// Scan a small box around the local player for unclaimed saplings.
fn scan_saplings(game: &mut Game<'_>, trees: &mut Trees, tick: i64) {
    if trees.growing.len() >= MAX_CLAIMS {
        return;
    }
    let Some(player) = game.player.as_ref() else {
        return;
    };
    let px = player.position.x.floor() as i32;
    let py = player.position.y.floor() as i32;
    let pz = player.position.z.floor() as i32;

    for x in (px - SCAN_HALF_XZ)..=(px + SCAN_HALF_XZ) {
        for z in (pz - SCAN_HALF_XZ)..=(pz + SCAN_HALF_XZ) {
            for y in (py - SCAN_DOWN)..=(py + SCAN_UP) {
                let pos = [x, y, z];
                if trees.growing.contains_key(&pos) {
                    continue;
                }
                let state = block_at(game, pos);
                if game.world.is_air(state) {
                    continue;
                }
                let name = name_of(game, state);
                if name == "minecraft:bamboo_sapling"
                    || (!name.ends_with("_sapling") && !name.ends_with("propagule"))
                {
                    continue;
                }
                if !in_tag(game, "minecraft:saplings", &name) {
                    continue;
                }
                if !has_stage(game, state) {
                    continue; // azaleas have no stage: vanilla handles them
                }
                let Some(trunk) = trunk_candidates(game, &name) else {
                    continue;
                };
                let (leaf, leaf_props) = canopy_of(game, &name);
                if leaf.is_none() {
                    continue;
                }
                let (hmin, hmax) = trunk_height(&name);
                let roll = tree_rand(pos, tick, 1) % (hmax - hmin + 1).max(1) as u64;
                let height = hmin + roll as i32;
                let tree = build_tree(pos, &name, trunk, leaf, leaf_props, height, tick);
                trees.growing.insert(pos, tree);
                if trees.growing.len() >= MAX_CLAIMS {
                    return;
                }
            }
        }
    }
}

/// Precompute the trunk column and the canopy shape for one tree.
fn build_tree(
    pos: [i32; 3],
    sapling: &str,
    trunk: BlockStateId,
    leaf: Option<BlockStateId>,
    leaf_props: bool,
    height: i32,
    tick: i64,
) -> Growing {
    let mut trunk_cells = Vec::with_capacity(height as usize + 1);
    for i in 0..=height {
        trunk_cells.push([pos[0], pos[1] + i, pos[2]]);
    }

    // The classic oak blob: two 5x5 layers under the top, then two 3x3 caps,
    // corners trimmed the way vanilla's oak does.
    let top = pos[1] + height;
    let mut leaf_cells: Vec<([i32; 3], u8)> = Vec::new();
    let mut seen = HashSet::new();
    let jitter = tree_rand(pos, tick, 2);
    for (layer, (dy, radius)) in [(-2i32, 2i32), (-1, 2), (0, 1), (1, 1)].into_iter().enumerate() {
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                if dx == 0 && dz == 0 && dy <= 0 {
                    continue; // the trunk keeps its column
                }
                let corner = dx.abs() == radius && dz.abs() == radius;
                if corner && radius == 1 {
                    continue;
                }
                let shift = (layer * 4 + ((dx + dz).rem_euclid(4)).unsigned_abs() as usize) as u32;
                if corner && (jitter >> shift) & 1 == 1 {
                    continue;
                }
                let cell = [pos[0] + dx, top + dy, pos[2] + dz];
                if seen.insert(cell) {
                    let distance = (dx.abs().max(dz.abs()).max(-dy) as u8).clamp(1, 4);
                    leaf_cells.push((cell, if leaf_props { distance } else { 1 }));
                }
            }
        }
    }

    Growing {
        pos,
        sapling: sapling.to_owned(),
        trunk,
        leaf,
        trunk_cells,
        leaf_cells,
        trunk_done: 0,
        leaf_done: 0,
        phase: 0,
        next_step: tick + GROW_DELAY,
    }
}

// ---------------------------------------------------------------------------
// Toppling: break a trunk and the whole tree falls with it.
// ---------------------------------------------------------------------------

/// The 26 neighbours of a cell, precomputed.
const NEIGHBORS_26: [(i32, i32, i32); 26] = {
    let mut out = [(0i32, 0i32, 0i32); 26];
    let mut i = 0;
    let mut dx = -1;
    while dx <= 1 {
        let mut dy = -1;
        while dy <= 1 {
            let mut dz = -1;
            while dz <= 1 {
                if dx != 0 || dy != 0 || dz != 0 {
                    out[i] = (dx, dy, dz);
                    i += 1;
                }
                dz += 1;
            }
            dy += 1;
        }
        dx += 1;
    }
    out
};

/// `Some(Ok(()))`: the tree fell, the original single break is cancelled.
fn fell_tree(game: &mut Game<'_>, pos: [i32; 3]) -> Option<Result<()>> {
    let clicked = name_of(game, block_at(game, pos));
    if !is_trunk_block(game, &clicked) {
        return None;
    }

    // The trunk: every trunk-tagged block connected through the 26 neighbours.
    let mut logs = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    seen.insert(pos);
    queue.push_back(pos);
    while let Some(cell) = queue.pop_front() {
        logs.push(cell);
        if logs.len() >= TOPPLE_MAX_LOGS {
            break;
        }
        for (dx, dy, dz) in NEIGHBORS_26 {
            let next = [cell[0] + dx, cell[1] + dy, cell[2] + dz];
            if seen.insert(next)
                && is_trunk_block(game, &name_of(game, block_at(game, next)))
            {
                queue.push_back(next);
            }
        }
    }

    // A tree, not a log pillar: leaves must touch the trunk somewhere.
    let mut leaf_front: VecDeque<([i32; 3], usize)> = VecDeque::new();
    let mut leaf_seen = HashSet::new();
    for cell in &logs {
        for (dx, dy, dz) in NEIGHBORS_26 {
            let next = [cell[0] + dx, cell[1] + dy, cell[2] + dz];
            if leaf_seen.insert(next)
                && is_leaf_block(game, &name_of(game, block_at(game, next)))
            {
                leaf_front.push_back((next, 1));
            }
        }
    }
    if leaf_front.is_empty() {
        return None; // bare logs: vanilla's single break is the right answer
    }

    // Collect the leaf ball around the trunk, a few layers deep at most.
    let mut leaves = Vec::new();
    while let Some((cell, depth)) = leaf_front.pop_front() {
        leaves.push(cell);
        if leaves.len() >= TOPPLE_MAX_LEAVES {
            break;
        }
        if depth >= TOPPLE_LEAF_DEPTH {
            continue;
        }
        for (dx, dy, dz) in NEIGHBORS_26 {
            let next = [cell[0] + dx, cell[1] + dy, cell[2] + dz];
            if leaf_seen.insert(next)
                && is_leaf_block(game, &name_of(game, block_at(game, next)))
            {
                leaf_front.push_back((next, depth + 1));
            }
        }
    }

    // Break the canopy first, then the trunk top-down: the tree falls.
    leaves.sort_by(|a, b| b[1].cmp(&a[1]).then(b.cmp(a)));
    logs.sort_by(|a, b| b[1].cmp(&a[1]).then(b.cmp(a)));
    for cell in leaves.iter().chain(logs.iter()) {
        if let Err(error) = game.break_block_as_player(*cell) {
            return Some(Err(error));
        }
    }
    Some(Ok(()))
}

fn is_trunk_block(game: &Game<'_>, name: &str) -> bool {
    in_tag(game, "minecraft:logs", name) || in_tag(game, "dynamictrees:branches", name)
}

fn is_leaf_block(game: &Game<'_>, name: &str) -> bool {
    in_tag(game, "minecraft:leaves", name) || in_tag(game, "dynamictrees:leaves", name)
}
