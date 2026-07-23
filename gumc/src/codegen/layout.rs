use crate::ast::*;
use crate::semantic::TypeChecker;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

// One field's committed on-chain position, as persisted in the storage lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldLoc {
    pub slot: usize,
    pub offset: usize,
    pub size: usize,
    #[serde(rename = "type")]
    pub type_name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClassLayout {
    pub fields: BTreeMap<String, FieldLoc>,
}

// The storage-layout lockfile. Written at deploy time and thereafter treated
// as immutable ground truth: existing fields keep their exact slot/offset
// across recompiles (so an upgraded implementation reads the same storage the
// proxy already holds), new fields are appended into free space, and any
// change that would move or reinterpret a committed field is a hard error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageManifest {
    pub version: u32,
    pub classes: BTreeMap<String, ClassLayout>,
}

impl StorageManifest {
    pub fn load(path: &str) -> Result<Option<StorageManifest>, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| format!("Could not parse storage lock '{}': {}", path, e)),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("Could not read storage lock '{}': {}", path, e)),
        }
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Could not serialize storage lock: {}", e))?;
        std::fs::write(path, json + "\n")
            .map_err(|e| format!("Could not write storage lock '{}': {}", path, e))
    }
}

// Where a storage-backed field lives: a 32-byte-aligned slot number, plus
// the byte range within that slot it occupies (so several small fields can
// share one slot, the classic Solidity-style packing optimization).
#[derive(Debug, Clone, Copy)]
pub struct StorageField {
    pub slot: usize,
    pub offset_in_slot: usize,
    pub size: usize,
    // Transient storage (EIP-1153) rather than persistent. THey have separate
    // keyspace, so this slot number is only meaningful together with this
    // flag, tslot 0 and sslot 0 are different locations.
    pub is_transient: bool,
}

// Where a memory-backed (non-global class instance) field lives: a byte
// offset from the struct's base pointer, plus its width. Memory is
// byte-addressable, so unlike storage this needs no slot bookkeeping
// fields are packed back-to-back with no padding.
#[derive(Debug, Clone, Copy)]
pub struct MemoryField {
    pub offset: usize,
    pub size: usize,
}

pub struct LayoutEngine<'a> {
    pub type_checker: &'a TypeChecker,
    pub storage_fields: HashMap<String, StorageField>,
    pub memory_fields: HashMap<String, MemoryField>,
    // Total packed size of each class's own memory representation, keyed by
    // class name. Used by size_of instead of a naive "32 bytes per field".
    packed_class_size: HashMap<String, usize>,
    // Per-class relative storage layout (slots numbered from 0), keyed
    // "Class.field". Used for structs living in storage, e.g. a struct value
    // inside a mapping, where each field sits at base_slot + this slot, with
    // this offset/size within that slot. Distinct from storage_fields, which
    // is the absolute layout of global singleton classes.
    struct_storage_fields: HashMap<String, StorageField>,
    // If a storage lock was supplied, this is the layout actually used
    // committed fields honored, new fields placed, ready to be written back.
    pub manifest_out: StorageManifest,
}

impl<'a> LayoutEngine<'a> {
    pub fn new(type_checker: &'a TypeChecker) -> Self {
        Self::with_lock(type_checker, None).expect("unlocked layout cannot fail")
    }

    pub fn with_lock(
        type_checker: &'a TypeChecker,
        lock: Option<StorageManifest>,
    ) -> Result<Self, String> {
        let mut engine = Self {
            type_checker,
            storage_fields: HashMap::new(),
            memory_fields: HashMap::new(),
            packed_class_size: HashMap::new(),
            struct_storage_fields: HashMap::new(),
            manifest_out: StorageManifest {
                version: 1,
                classes: BTreeMap::new(),
            },
        };
        engine.allocate_memory_layouts();
        engine.allocate_struct_layouts();
        engine.allocate_storage(lock.as_ref())?;
        Ok(engine)
    }

    // Relative storage layout for every class (slots from 0), so a struct held
    // in storage, e.g. as a mapping's value, knows each field's slot offset
    // and packing. Same packer as global storage, just base slot 0.
    fn allocate_struct_layouts(&mut self) {
        for (class_name, class_decl) in self.ordered_classes() {
            let (fields, _next) = self.pack_storage_fields(&class_decl.fields, 0, false);
            for (fname, sf) in fields {
                self.struct_storage_fields
                    .insert(format!("{}.{}", class_name, fname), sf);
            }
        }
    }

    pub fn struct_storage_field(&self, class_name: &str, property: &str) -> Option<StorageField> {
        self.struct_storage_fields
            .get(&format!("{}.{}", class_name, property))
            .copied()
    }

    // Real byte width of a scalar integer/bool type. Everything else (u256,
    // i256, f32/f64, these represent full-width fixed-point values, not
    // native 32/64-bit floats, classes, enums, arrays, generics) keeps
    fn scalar_byte_width(type_def: &Type) -> Option<usize> {
        if let Type::Primitive(name) = type_def {
            return match name.as_str() {
                "u8" | "i8" | "bool" => Some(1),
                "u16" | "i16" => Some(2),
                "u32" | "i32" => Some(4),
                "u64" | "i64" => Some(8),
                "u128" | "i128" => Some(16),
                _ => None,
            };
        }
        None
    }

    fn field_byte_width(&self, type_def: &Type) -> usize {
        if matches!(type_def, Type::Primitive(n) if n == "String" || n == "Bytes") {
            return 32;
        }
        if let Type::FixedArray(inner, n) = type_def {
            let bytes = self.size_of(inner) * n;
            return ((bytes + 31) / 32).max(1) * 32;
        }
        Self::scalar_byte_width(type_def).unwrap_or_else(|| self.size_of(type_def))
    }

    // Shared by both packers: order fields largest-first (stable on
    // declaration order for ties) so the greedy bin-fill below packs well
    // without needing a smarter bin-packing algorithm.
    fn size_sorted_indices(&self, fields: &[ClassField]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..fields.len()).collect();
        order.sort_by(|&a, &b| {
            let wa = self.field_byte_width(&fields[a].type_def);
            let wb = self.field_byte_width(&fields[b].type_def);
            wb.cmp(&wa).then(a.cmp(&b))
        });
        order
    }

    // Packs a class's fields into 32-byte storage slots, largest fields
    // first, filling in smaller ones wherever they still fit, this is the
    // manual optimization Solidity asks the author to do by hand; here the
    fn pack_storage_fields(
        &self,
        fields: &[ClassField],
        start_slot: usize,
        is_transient: bool,
    ) -> (HashMap<String, StorageField>, usize) {
        let mut layout = HashMap::new();
        let mut slot = start_slot;
        let mut used = 0usize;
        for idx in self.size_sorted_indices(fields) {
            let f = &fields[idx];
            let w = self.field_byte_width(&f.type_def);
            if w > 32 {
                if used > 0 {
                    slot += 1;
                    used = 0;
                }
                layout.insert(
                    f.name.clone(),
                    StorageField {
                        slot,
                        offset_in_slot: 0,
                        size: w,
                        is_transient,
                    },
                );
                slot += (w + 31) / 32;
                continue;
            }
            if used + w > 32 {
                slot += 1;
                used = 0;
            }
            layout.insert(
                f.name.clone(),
                StorageField {
                    slot,
                    offset_in_slot: used,
                    size: w,
                    is_transient,
                },
            );
            used += w;
        }
        let next_free_slot = if used > 0 { slot + 1 } else { slot };
        (layout, next_free_slot)
    }

    // Packs a class's fields into a contiguous memory buffer, largest first.
    // No slot alignment needed, mload/mstore can address any byte offset.
    fn pack_memory_fields(&self, fields: &[ClassField]) -> (HashMap<String, MemoryField>, usize) {
        let mut layout = HashMap::new();
        let mut offset = 0usize;
        for idx in self.size_sorted_indices(fields) {
            let f = &fields[idx];
            let w = self.field_byte_width(&f.type_def);
            layout.insert(f.name.clone(), MemoryField { offset, size: w });
            offset += w;
        }
        (layout, offset)
    }

    // Both passes below walk class_order, the order classes were first
    // registered in (declaration order locally, then import order), rather
    // than loaded_classes directly. loaded_classes is a HashMap, and Rust
    fn ordered_classes(&self) -> Vec<(String, ClassDecl)> {
        self.type_checker
            .class_order
            .iter()
            .filter_map(|name| {
                self.type_checker
                    .loaded_classes
                    .get(name)
                    .map(|c| (name.clone(), c.clone()))
            })
            .collect()
    }

    fn allocate_memory_layouts(&mut self) {
        for (class_name, class_decl) in self.ordered_classes() {
            let (fields, total) = self.pack_memory_fields(&class_decl.fields);
            for (fname, mf) in fields {
                self.memory_fields
                    .insert(format!("{}.{}", class_name, fname), mf);
            }
            self.packed_class_size
                .insert(class_name, if total == 0 { 32 } else { total });
        }
    }

    fn allocate_storage(&mut self, lock: Option<&StorageManifest>) -> Result<(), String> {
        for (class_name, class_decl) in self.ordered_classes() {
            if !class_decl.is_global || class_name == "Message" || class_name == "Block" {
                continue;
            }

            let mut append = 0usize;
            if let Some(cl) = lock.and_then(|m| m.classes.get(&class_name)) {
                for fl in cl.fields.values() {
                    append = append.max(fl.slot + ((fl.offset + fl.size + 31) / 32).max(1));
                }
            }

            let (persistent, transient): (Vec<ClassField>, Vec<ClassField>) = class_decl
                .fields
                .iter()
                .filter(|f| !f.is_const)
                .cloned()
                .partition(|f| !f.is_transient);

            let mut layout = match lock.and_then(|m| m.classes.get(&class_name)) {
                Some(committed) => self.pack_storage_fields_locked(
                    &class_name,
                    &persistent,
                    committed,
                    &mut append,
                )?,
                // The returned next-free slot is discarded: append is per-class, and the transient pass below starts its own keyspace at 0.
                None => self.pack_storage_fields(&persistent, append, false).0,
            };
            let (tlayout, _) = self.pack_storage_fields(&transient, 0, true);
            layout.extend(tlayout);

            let mut class_out = ClassLayout::default();
            for f in &class_decl.fields {
                if f.is_transient || f.is_const {
                    continue;
                }
                if let Some(sf) = layout.get(&f.name) {
                    class_out.fields.insert(
                        f.name.clone(),
                        FieldLoc {
                            slot: sf.slot,
                            offset: sf.offset_in_slot,
                            size: sf.size,
                            type_name: super::type_suffix(&f.type_def),
                        },
                    );
                }
            }
            self.manifest_out
                .classes
                .insert(class_name.clone(), class_out);

            for (fname, sf) in layout {
                self.storage_fields
                    .insert(format!("{}.{}", class_name, fname), sf);
            }
        }
        Ok(())
    }

    // Packs a class under an existing storage lock: every committed field keeps
    // its exact slot/offset; new fields are placed into leftover space (a tail
    // gap in a committed slot, or a fresh slot beyond all committed storage).
    fn pack_storage_fields_locked(
        &self,
        class_name: &str,
        fields: &[ClassField],
        committed: &ClassLayout,
        append: &mut usize,
    ) -> Result<HashMap<String, StorageField>, String> {
        let current: std::collections::HashSet<&str> =
            fields.iter().map(|f| f.name.as_str()).collect();
        for fname in committed.fields.keys() {
            if !current.contains(fname.as_str()) {
                return Err(format!(
                    "Storage-lock violation in '{}': committed field '{}' was removed. Existing storage would be orphaned, removing a locked field is unsafe for an upgrade. Re-add it (or start a new lock for a fresh deployment).",
                    class_name, fname
                ));
            }
        }

        let mut layout: HashMap<String, StorageField> = HashMap::new();
        let mut high_water: BTreeMap<usize, usize> = BTreeMap::new();
        let mut newcomers: Vec<&ClassField> = Vec::new();

        for f in fields {
            let w = self.field_byte_width(&f.type_def);
            match committed.fields.get(&f.name) {
                Some(fl) => {
                    if fl.size != w {
                        return Err(format!(
                            "Storage-lock violation in '{}': field '{}' changed size ({} -> {} bytes). This would move or overlap committed storage; keep its type stable across an upgrade.",
                            class_name, f.name, fl.size, w
                        ));
                    }
                    layout.insert(
                        f.name.clone(),
                        StorageField {
                            slot: fl.slot,
                            offset_in_slot: fl.offset,
                            size: fl.size,
                            is_transient: false,
                        },
                    );
                    let spanned = ((fl.offset + fl.size + 31) / 32).max(1);
                    for i in 0..spanned {
                        let used = if i + 1 == spanned {
                            (fl.offset + fl.size) - i * 32
                        } else {
                            32
                        };
                        let e = high_water.entry(fl.slot + i).or_insert(0);
                        *e = (*e).max(used.min(32));
                    }
                }
                None => newcomers.push(f),
            }
        }

        newcomers.sort_by(|a, b| {
            self.field_byte_width(&b.type_def)
                .cmp(&self.field_byte_width(&a.type_def))
                .then(a.name.cmp(&b.name))
        });
        for f in newcomers {
            let w = self.field_byte_width(&f.type_def);
            if w > 32 {
                let slot = *append;
                *append += (w + 31) / 32;
                layout.insert(
                    f.name.clone(),
                    StorageField {
                        slot,
                        offset_in_slot: 0,
                        size: w,
                        is_transient: false,
                    },
                );
                continue;
            }
            let gap_slot = high_water
                .iter()
                .filter(|(_, hw)| 32 - **hw >= w)
                .map(|(s, _)| *s)
                .min();
            let (slot, offset) = match gap_slot {
                Some(s) => (s, high_water[&s]),
                None => {
                    let s = *append;
                    *append += 1;
                    (s, 0)
                }
            };
            layout.insert(
                f.name.clone(),
                StorageField {
                    slot,
                    offset_in_slot: offset,
                    size: w,
                    is_transient: false,
                },
            );
            *high_water.entry(slot).or_insert(0) = offset + w;
        }

        Ok(layout)
    }

    // Computes the memory size (in bytes) of a given type.
    // This groups variables into structs so we avoid the 16-var EVM stack limit.
    pub fn size_of(&self, type_def: &Type) -> usize {
        match type_def {
            Type::Primitive(name) => {
                if let Some(w) = Self::scalar_byte_width(type_def) {
                    w
                } else if name == "u256" || name == "i256" || name == "f32" || name == "f64" {
                    // full EVM word
                    32
                } else if let Some(&packed) = self.packed_class_size.get(name) {
                    packed
                } else if self.type_checker.loaded_classes.contains_key(name) {
                    // class known but not yet laid out
                    32
                } else if self.type_checker.loaded_enums.contains_key(name) {
                    // A payload-free enum is a tag and nothing else, so it is one byte, exactly as Solidity lays an enum out. Saying 64 here was the root of a family of bugs: it burned two storage slots instead of one byte, displaced every field after it, and made the mapping/log paths write the memory pointer instead of the value.
                    // A payload-carrying enum keeps the [tag][payload] pair, which only exists in memory; it is rejected anywhere that needs a size, so this number is never used to lay one out.
                    if self.type_checker.enum_has_payload(name) {
                        64
                    } else {
                        1
                    }
                } else {
                    32
                }
            }
            Type::FixedArray(inner, size) => self.size_of(inner) * size,
            Type::Array(_) => {
                // dynamic arrays are a 32-byte pointer
                32
            }
            Type::Generic { .. } => 32,
        }
    }

    pub fn storage_field(&self, class_name: &str, property: &str) -> Option<StorageField> {
        self.storage_fields
            .get(&format!("{}.{}", class_name, property))
            .copied()
    }

    pub fn memory_field(&self, class_name: &str, property: &str) -> Option<MemoryField> {
        self.memory_fields
            .get(&format!("{}.{}", class_name, property))
            .copied()
    }

    // The type of class_name.property if it is an immutable field.
    //
    // Immutables own no slot, so storage_field returns None for them, the
    pub fn immutable_field(&self, class_name: &str, property: &str) -> Option<Type> {
        let class = self.type_checker.loaded_classes.get(class_name)?;
        class
            .fields
            .iter()
            .find(|f| f.is_const && f.name == property)
            .map(|f| f.type_def.clone())
    }

    // Every const field of a contract, in declaration order.
    pub fn immutable_fields(&self, class_name: &str) -> Vec<ClassField> {
        self.type_checker
            .loaded_classes
            .get(class_name)
            .map(|c| c.fields.iter().filter(|f| f.is_const).cloned().collect())
            .unwrap_or_default()
    }

    // The const fields that still need a deploy-time patch, i.e. those the
    // compiler could not fold to a compile-time value.
    //
    pub fn patched_immutables(&self, class_name: &str) -> Vec<String> {
        self.immutable_fields(class_name)
            .iter()
            .filter(|f| self.const_field_value(class_name, &f.name).is_none())
            .map(|f| f.name.clone())
            .collect()
    }

    // The compile-time value of a const field, when the constructor gives it
    pub fn const_field_value(&self, class_name: &str, field: &str) -> Option<String> {
        let class = self.type_checker.loaded_classes.get(class_name)?;
        let f = class
            .fields
            .iter()
            .find(|f| f.is_const && f.name == field)?;
        let ctor = class.methods.iter().find(|m| m.name == "new")?;

        let assignments = count_assignments(&ctor.body, class_name, field);
        if assignments != 1 {
            return None;
        }
        let value = ctor.body.iter().find_map(|s| match &s.node {
            Statement::Assignment { target, value } if targets(target, class_name, field) => {
                Some(value)
            }
            _ => None,
        })?;

        let n = match value {
            Expr::Number(text) => text.parse::<u128>().ok()?,
            _ => return None,
        };

        let (bits, signed) = match &f.type_def {
            Type::Primitive(name) => numeric_meta(name)?,
            _ => return None,
        };
        if signed {
            // two's-complement bounds need their own reasoning
            return None;
        }
        if bits < 128 && n > (1u128 << bits) - 1 {
            return None;
        }
        Some(n.to_string())
    }
}

// Whether e names Class.field (or self.field inside a method).
fn targets(e: &Expr, class_name: &str, field: &str) -> bool {
    match e {
        Expr::PropertyAccess { base, property } => {
            property == field
                && matches!(&**base, Expr::Identifier(b) if b == class_name || b == "self")
        }
        _ => false,
    }
}

// How many times body assigns Class.field, at any depth.
fn count_assignments(body: &[Spanned<Statement>], class_name: &str, field: &str) -> usize {
    body.iter()
        .map(|s| match &s.node {
            Statement::Assignment { target, .. } if targets(target, class_name, field) => 1,
            Statement::IfElse {
                if_body, else_body, ..
            } => {
                count_assignments(if_body, class_name, field)
                    + else_body
                        .as_ref()
                        .map_or(0, |b| count_assignments(b, class_name, field))
            }
            Statement::WhileLoop { body, .. } | Statement::ForLoop { body, .. } => {
                count_assignments(body, class_name, field)
            }
            Statement::Match { arms, .. } => arms
                .iter()
                .map(|a| count_assignments(&a.body, class_name, field))
                .sum(),
            _ => 0,
        })
        .sum()
}

// Width and signedness of a primitive integer type name.
fn numeric_meta(name: &str) -> Option<(usize, bool)> {
    match name {
        "u8" => Some((8, false)),
        "u16" => Some((16, false)),
        "u32" => Some((32, false)),
        "u64" => Some((64, false)),
        "u128" => Some((128, false)),
        "u256" => Some((256, false)),
        "i8" | "i16" | "i32" | "i64" | "i128" | "i256" => Some((0, true)),
        _ => None,
    }
}

// The Yul loadimmutable/setimmutable key for a field. Namespaced by class
// so two contracts in one file (one nested inside the other's object) cannot
// collide on a bare field name.
pub fn immutable_key(class_name: &str, field: &str) -> String {
    format!("{}_{}", class_name, field)
}
