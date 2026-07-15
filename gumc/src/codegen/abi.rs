use crate::ast::*;
use crate::semantic::TypeChecker;
use serde::{Deserialize, Serialize};
use tiny_keccak::{Hasher, Keccak};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AbiInput {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub components: Vec<AbiInput>,
    // Event fields only: whether this field is a LOG topic rather than data.
    // None for function and constructor inputs, where the key must be absent because indexed is not part of their ABI shape.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub indexed: Option<bool>,
}

impl AbiInput {
    pub fn plain(name: String, type_name: String, components: Vec<AbiInput>) -> Self {
        Self { name, type_name, components, indexed: None }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AbiEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "function", "constructor", "error", "event"
    pub name: String,
    pub inputs: Vec<AbiInput>,
    // Option rather than always emitted, so an event or error omits these keys entirely.
    // Serializing an empty outputs or a meaningless stateMutability would make the entry invalid to a strict decoder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<AbiInput>>,
    #[serde(rename = "stateMutability", skip_serializing_if = "Option::is_none")]
    pub state_mutability: Option<String>, // "nonpayable", "view", "pure", "payable"
    // Events only, and always Some(false): gum has no anonymous event syntax, but the key is required for the entry to be well formed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anonymous: Option<bool>,
}

pub struct AbiGenerator<'a> {
    type_checker: &'a TypeChecker,
}

impl<'a> AbiGenerator<'a> {
    pub fn new(type_checker: &'a TypeChecker) -> Self {
        Self { type_checker }
    }

    // Account maps to address rather than a tuple, because its u256 field is only a storage convenience and codegen masks it to 160 bits.
    // As a tuple the signature would read (uint256) and no standard caller could compute the selector.
    pub fn map_type(&self, type_def: &Type) -> String {
        match type_def {
            Type::Primitive(name) => {
                match name.as_str() {
                    "u8" | "u16" | "u32" | "u64" | "u128" | "u256" => format!("uint{}", name[1..].to_string()),
                    "i8" | "i16" | "i32" | "i64" | "i128" | "i256" => format!("int{}", name[1..].to_string()),
                    "Address" | "Account" => "address".to_string(),
                    "bool" => "bool".to_string(),
                    "f32" | "f64" => "int256".to_string(), // Default mapped to fixed math sizes
                    "Message" => "Message".to_string(), // Handled by ABI filtering
                    "Block" => "Block".to_string(), // Handled by ABI filtering
                    _ => {
                        if name == "String" {
                            "string".to_string()
                        } else if name == "Bytes" {
                            "bytes".to_string()
                        } else if self.type_checker.loaded_classes.contains_key(name) {
                            "tuple".to_string()
                        } else if self.type_checker.loaded_enums.contains_key(name) {
                            "uint8".to_string()
                        } else {
                            "uint256".to_string()
                        }
                    }
                }
            }
            Type::Array(inner) => format!("{}[]", self.map_type(inner)),
            Type::FixedArray(inner, size) => format!("{}[{}]", self.map_type(inner), size),
            _ => "uint256".to_string(),
        }
    }

    // The type as it appears in a function signature, where a struct is spelled out as its component tuple.
    // map_type gives the JSON ABI spelling, which keeps the word "tuple" and moves the fields into a components list; a selector needs the expanded form or no standard caller could compute it.
    // Recursive rather than a special case for a bare tuple, since the element of an array can be one too and (uint128,uint256)[] is not the same string as tuple[].
    pub fn signature_type(&self, type_def: &Type) -> String {
        match type_def {
            Type::Array(inner) => format!("{}[]", self.signature_type(inner)),
            Type::FixedArray(inner, n) => format!("{}[{}]", self.signature_type(inner), n),
            Type::Primitive(name) => {
                let base = self.map_type(type_def);
                if base == "tuple" {
                    if let Some(cd) = self.type_checker.loaded_classes.get(name) {
                        let parts: Vec<String> = cd
                            .fields
                            .iter()
                            .map(|f| self.signature_type(&f.type_def))
                            .collect();
                        return format!("({})", parts.join(","));
                    }
                }
                base
            }
            _ => self.map_type(type_def),
        }
    }

    // Unwraps to the element type first, so an array of structs still reports the struct's fields; the JSON for tuple[] carries the same components as tuple.
    pub fn generate_components(&self, type_def: &Type) -> Vec<AbiInput> {
        if let Type::Array(inner) | Type::FixedArray(inner, _) = type_def {
            return self.generate_components(inner);
        }
        if let Type::Primitive(name) = type_def {
            if name == "String" || name == "Bytes" {
                return Vec::new();
            }
            if let Some(class_decl) = self.type_checker.loaded_classes.get(name) {
                let mut components = Vec::new();
                for field in &class_decl.fields {
                    components.push(AbiInput::plain(
                        field.name.clone(),
                        self.map_type(&field.type_def),
                        self.generate_components(&field.type_def),
                    ));
                }
                return components;
            }
        }
        Vec::new()
    }

    // The ABI JSON for one contract: constructor, exported functions and errors, with events appended by the caller from the translator's registry.
    // Non-export fns are skipped to match the dispatcher, which gives them no selector.
    // receive and fallback get their own entry types, never a function entry, or a caller would compute a selector that dispatches nowhere.
    pub fn generate_abi(&self, program: &Program, class: &ClassDecl) -> Vec<AbiEntry> {
        let mut entries = Vec::new();

        if let Some(constructor) = class.methods.iter().find(|m| m.name == "new") {
            let mut inputs = Vec::new();
            for param in &constructor.parameters {
                let type_name = self.map_type(&param.type_def);
                if type_name == "Message" || type_name == "Block" {
                    continue;
                }
                
                inputs.push(AbiInput::plain(
                    param.name.clone(),
                    type_name.clone(),
                    self.generate_components(&param.type_def),
                ));
            }

            entries.push(AbiEntry {
                entry_type: "constructor".to_string(),
                name: "".to_string(), // constructors do not have a name in ABI
                inputs,
                outputs: Some(Vec::new()),
                state_mutability: Some("nonpayable".to_string()),
                anonymous: None,
            });
        }

        for f in &class.methods {
            if !f.modifiers.iter().any(|m| m == "export") {
                    continue;
                }
                if f.name == "receive" || f.name == "fallback" {
                    entries.push(AbiEntry {
                        entry_type: f.name.clone(),
                        name: String::new(),
                        inputs: Vec::new(),
                        outputs: Some(Vec::new()),
                        state_mutability: Some(if f.modifiers.iter().any(|m| m == "payable") {
                            "payable".to_string()
                        } else {
                            "nonpayable".to_string()
                        }),
                        anonymous: None,
                    });
                    continue;
                }
                let mut inputs = Vec::new();
                for param in &f.parameters {
                    let type_name = self.map_type(&param.type_def);
                    if type_name == "Message" || type_name == "Block" {
                        continue;
                    }
                    
                    inputs.push(AbiInput::plain(
                        param.name.clone(),
                        type_name.clone(),
                        self.generate_components(&param.type_def),
                    ));
                }

                let mut outputs = Vec::new();
                if let Some(ret_type) = &f.return_type {
                    outputs.push(AbiInput::plain(
                        "".to_string(),
                        self.map_type(ret_type),
                        self.generate_components(ret_type),
                    ));
                }

                entries.push(AbiEntry {
                    entry_type: "function".to_string(),
                    name: f.name.clone(),
                    inputs,
                    outputs: Some(outputs),
                    state_mutability: Some(if f.modifiers.iter().any(|m| m == "payable") {
                        "payable".to_string()
                    } else {
                        "nonpayable".to_string()
                    }),
                    anonymous: None,
                });
        }

        for decl in &program.declarations {
            if let Declaration::Error(err) = decl {
                let mut inputs = Vec::new();
                for param in &err.parameters {
                    inputs.push(AbiInput::plain(
                        param.name.clone(),
                        self.map_type(&param.type_def),
                        self.generate_components(&param.type_def),
                    ));
                }
                entries.push(AbiEntry {
                    entry_type: "error".to_string(),
                    name: err.name.clone(),
                    inputs,
                    outputs: None,
                    state_mutability: None,
                    anonymous: None,
                });
            }
        }

        entries
    }

    // Builds the ABI entry for one event, from the schema the translator
    // recorded at its log() site. Kept here so every ABI shape decision
    // lives in one file, but the *contents* come from codegen, see
    // Translator::record_event.
    pub fn event_entry(name: &str, inputs: Vec<AbiInput>) -> AbiEntry {
        AbiEntry {
            entry_type: "event".to_string(),
            name: name.to_string(),
            inputs,
            outputs: None,
            state_mutability: None,
            anonymous: Some(false),
        }
    }

    // The 4-byte selector for an entry point, with any tuple expanded to its component types, e.g. (uint256,address).
    pub fn calculate_selector(&self, f: &FnDecl) -> String {
        let mut sig = format!("{}(", f.name);

        let param_types: Vec<String> = f.parameters.iter().filter_map(|p| {
            let base_type = self.map_type(&p.type_def);
            if base_type == "Message" || base_type == "Block" {
                return None;
            }
            Some(self.signature_type(&p.type_def))
        }).collect();
        
        sig.push_str(&param_types.join(","));
        sig.push(')');

        let mut keccak = Keccak::v256();
        let mut output = [0u8; 32];
        keccak.update(sig.as_bytes());
        keccak.finalize(&mut output);

        format!("0x{:02x}{:02x}{:02x}{:02x}", output[0], output[1], output[2], output[3])
    }

    pub fn calculate_error_selector(&self, err: &ErrorDecl) -> String {
        let mut sig = format!("{}(", err.name);
        
        let param_types: Vec<String> = err.parameters.iter()
            .map(|p| self.signature_type(&p.type_def))
            .collect();
        
        sig.push_str(&param_types.join(","));
        sig.push(')');

        let mut keccak = Keccak::v256();
        let mut output = [0u8; 32];
        keccak.update(sig.as_bytes());
        keccak.finalize(&mut output);

        format!("0x{:02x}{:02x}{:02x}{:02x}", output[0], output[1], output[2], output[3])
    }
}
