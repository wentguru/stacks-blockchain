use vm::errors::{RuntimeErrorType, InterpreterResult, InterpreterError};
use vm::types::{Value, OptionalData, PrincipalData, TypeSignature, AtomTypeIdentifier, TupleData};
use vm::database::{ClaritySerializable, ClarityDeserializable};

use std::collections::HashMap;
use serde_json::{Value as JSONValue};
use util::hash;

const TYPE_I128: &str = "i128";
const TYPE_BOOL: &str = "bool";
const TYPE_BUFF: &str = "buff";
const TYPE_STANDARD_PRINCIPAL: &str = "principal";
const TYPE_CONTRACT_PRINCIPAL: &str = "contract_principal";
const TYPE_RESP_OK: &str = "ok";
const TYPE_RESP_ERR: &str = "err";
const TYPE_OPT_SOME: &str = "some";
const TYPE_OPT_NONE: &str = "none";
const TYPE_TUPLE: &str = "tuple";
const TYPE_LIST: &str = "list";

enum ContainerTypes { OPT_SOME, RESP_OK, RESP_ERR }

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum JSONParser {
    None {
        #[serde(rename="type")]
        type_n: String },
    Bool { 
        #[serde(rename="type")]
        type_n: String,
        value: bool },
    Simple { 
        #[serde(rename="type")]
        type_n: String,
        value: String },
    Container {
        #[serde(rename="type")]
        type_n: String,
        value: Box<JSONParser> },
    List {
        #[serde(rename="type")]
        type_n: String,
        entries: Vec<JSONParser> },
    Tuple {
        #[serde(rename="type")]
        type_n: String,
        entries: HashMap<String, JSONParser> },
    ContractPrincipal {
        #[serde(rename="type")]
        type_n: String,
        issuer: String,
        name: String
    }
}

fn to_hex(val: &i128) -> String {
    if *val >= 0 {
        format!("{:x}", val)
    } else {
        format!("-{:x}", -val)
    }
}

fn json_simple_object(type_name: &str, val: &str) -> String {
    format!(
        r#"{{ "type": "{}", "value": "{}" }}"#,
        type_name, val)
}

fn json_recursive_object(type_name: &str, val: &str) -> String {
    format!(
        r#"{{ "type": "{}", "value": {} }}"#,
        type_name, val)
}

impl ClaritySerializable for Value {
    fn serialize(&self) -> String {
        use super::Value::*;
        use super::PrincipalData::*;

        match self {
            Int(value) => json_simple_object(TYPE_I128, &to_hex(value)),
            Buffer(value) => json_simple_object(TYPE_BUFF, &hash::bytes_to_hex(&value.data)),
            Bool(value) => {
                let str_value = if *value {
                    "true"
                } else {
                    "false"
                };
                json_recursive_object(TYPE_BOOL, str_value)
            },
            Principal(StandardPrincipal(data)) => {
                json_simple_object(TYPE_STANDARD_PRINCIPAL, &data.to_address())
            },
            Principal(ContractPrincipal(simple_name)) => {
                // NOTE: this should eventually panic, since "unresolved" contract principals
                //       should never be materialized to the database.
                format!(
                    r#"{{ "type": "{}", "issuer": ":none:", "name": "{}" }}"#,
                    TYPE_CONTRACT_PRINCIPAL, simple_name)
            },
            Principal(QualifiedContractPrincipal{ sender, name }) => {
                format!(
                    r#"{{ "type": "{}", "issuer": "{}", "name": "{}" }}"#,
                    TYPE_CONTRACT_PRINCIPAL, sender.to_address(), name)
            },
            Response(response) => {
                let type_name = if response.committed {
                    TYPE_RESP_OK
                } else {
                    TYPE_RESP_ERR
                };
                let value = response.data.serialize();
                json_recursive_object(type_name, &value)
            },
            Optional(OptionalData{ data: None }) => {
                format!(r#"{{ "type": "{}" }}"#, TYPE_OPT_NONE)
            },
            Optional(OptionalData{ data: Some(value) }) => {
                json_recursive_object(TYPE_OPT_SOME, &value.serialize()) 
           },
            List(data) => {
                let entries: Vec<String> = data.data
                    .iter().map(|x| x.serialize())
                    .collect();
                let entries_str = entries.join(", ");
                format!(
                    r#"{{ "type": "{}", "entries": [ {} ] }}"#,
                    TYPE_LIST, entries_str)
            },
            Tuple(data) => {
                let entries: Vec<String> = data.data_map
                    .iter().map(|(key, value)|
                                format!(r#""{}": {}"#, key, value.serialize()))
                    .collect();
                let entries_str = entries.join(", ");
                format!(
                    r#"{{ "type": "{}", "entries": {{ {} }} }}"#,
                    TYPE_TUPLE, entries_str)
            }
        }
    }
}

macro_rules! check_match {
    ($item:expr, $Pattern:pat) => {
        match $item {
            None => Ok(()),
            Some(x) => match x {
                TypeSignature::Atom($Pattern) => Ok(()),
                _ => Err(InterpreterError::DeserializeExpected(x.clone()))
            }
        }
    }
}


impl Value {
    /// This function attempts to deserialize a JSONParser struct into a Clarity Value.
    ///   The `expected_type` parameter determines whether or not the deserializer should expect (and enforce)
    ///   a particular type. `ClarityDB` uses this to ensure that lists, tuples, etc. loaded from the database
    ///   have their max-length and other type information set by the type declarations in the contract.
    ///   If passed `None`, the deserializer will construct the values as if they were literals in the contract, e.g.,
    ///     list max length = the length of the list.
    fn try_deserialize_parsed(json: JSONParser, expected_type: Option<&TypeSignature>) -> InterpreterResult<Value> {
        match json {
            JSONParser::Simple { type_n, value } => {
                match type_n.as_str() {
                    TYPE_I128 => {
                        check_match!(expected_type, AtomTypeIdentifier::IntType)?;
                        let value = i128::from_str_radix(&value, 16)
                            .map_err(|e| RuntimeErrorType::ParseError("Failed to parse hexstring as integer".into()))?;
                        Ok(Value::Int(value))
                    },
                    TYPE_STANDARD_PRINCIPAL => {
                        check_match!(expected_type, AtomTypeIdentifier::PrincipalType)?;
                        PrincipalData::parse_standard_principal(&value)
                            .map(|principal| Value::from(principal))
                    },
                    TYPE_BUFF => {
                        let bytes = hash::hex_bytes(&value)
                            .map_err(|e| RuntimeErrorType::ParseError("Bad hex string".into()))?;

                        match expected_type {
                            None => {},
                            Some(x) => {
                                let passed_test = match x {
                                    TypeSignature::Atom(AtomTypeIdentifier::BufferType(buff_len)) => {
                                        bytes.len() <= (*buff_len as usize)
                                    },
                                    _ => false
                                };
                                if !passed_test {
                                    return Err(InterpreterError::DeserializeExpected(x.clone()).into())
                                }
                            }
                        };

                        Value::buff_from(bytes)
                    },
                    _ => Err(InterpreterError::DeserializeUnexpectedTypeField(type_n).into())
                }
            },
            JSONParser::Bool { type_n, value } => {
                check_match!(expected_type, AtomTypeIdentifier::BoolType)?;

                if type_n == TYPE_BOOL {
                    Ok(Value::Bool(value))
                } else {
                    Err(InterpreterError::DeserializeUnexpectedTypeField(type_n).into())
                }
            },
            JSONParser::None { type_n } => {
                check_match!(expected_type, AtomTypeIdentifier::OptionalType(_))?;
                if type_n == TYPE_OPT_NONE {
                    Ok(Value::none())
                } else {
                    Err(InterpreterError::DeserializeUnexpectedTypeField(type_n).into())
                }
            },
            JSONParser::Container { type_n, value } => {
                let outer_type = match type_n.as_str() {
                    TYPE_RESP_OK => Ok(ContainerTypes::RESP_OK),
                    TYPE_RESP_ERR => Ok(ContainerTypes::RESP_ERR),
                    TYPE_OPT_SOME => Ok(ContainerTypes::OPT_SOME),
                    _ => Err(InterpreterError::DeserializeUnexpectedTypeField(type_n))
                }?;

                let expect_contained_type = match expected_type {
                    None => None,
                    Some(x) => {
                        let contained_type = match (&outer_type, x) {
                            (ContainerTypes::RESP_OK,  TypeSignature::Atom(AtomTypeIdentifier::ResponseType(types))) => Ok(&types.0),
                            (ContainerTypes::RESP_ERR, TypeSignature::Atom(AtomTypeIdentifier::ResponseType(types))) => Ok(&types.1),
                            (ContainerTypes::OPT_SOME, TypeSignature::Atom(AtomTypeIdentifier::OptionalType(some_type))) => Ok(some_type.as_ref()),
                            _ => Err(InterpreterError::DeserializeExpected(x.clone()))
                        }?;
                        Some(contained_type)
                    }
                };

                let deserialized_value = Value::try_deserialize_parsed(*value, expect_contained_type)?;
                match outer_type {
                    ContainerTypes::RESP_OK => Ok(Value::okay(deserialized_value)),
                    ContainerTypes::RESP_ERR => Ok(Value::error(deserialized_value)),
                    ContainerTypes::OPT_SOME => Ok(Value::some(deserialized_value))
                }
            },
            JSONParser::List { type_n, mut entries } => {
                if type_n != TYPE_LIST {
                    return Err(InterpreterError::DeserializeUnexpectedTypeField(type_n).into())
                }

                let (list_type, entry_type) = match expected_type {
                    None => (None, None),
                    Some(x) => {
                        match x {
                            TypeSignature::List(list_type) => (Some(list_type), Some(list_type.get_list_item_type())),
                            _ => return Err(InterpreterError::DeserializeExpected(x.clone()).into())
                        }
                    }
                };

                let items: InterpreterResult<_> = entries
                    .drain(..)
                    .map(|value| Value::try_deserialize_parsed(value, entry_type.as_ref()))
                    .collect();

                if let Some(list_type) = list_type {
                    Value::list_with_type(items?, list_type.clone())
                } else {
                    Value::list_from(items?)
                }
            },
            JSONParser::Tuple { type_n, mut entries } => {
                if type_n != TYPE_TUPLE {
                    return Err(InterpreterError::DeserializeUnexpectedTypeField(type_n).into())
                }
                let tuple_type = match expected_type {
                    None => None,
                    Some(x) => {
                        match x {
                            TypeSignature::Atom(AtomTypeIdentifier::TupleType(tuple_type)) => Some(tuple_type),
                            _ => return Err(InterpreterError::DeserializeExpected(x.clone()).into())
                        }
                    }
                };

                let deserialized_entries: InterpreterResult<_> = entries
                    .drain()
                    .map(|(key, json_val)| {
                        let expected_field_type = match tuple_type {
                            None => None,
                            Some(some_tuple) => Some(
                                some_tuple
                                    .field_type(&key)
                                    .ok_or_else(|| RuntimeErrorType::ParseError(
                                        format!("Expected tuple type does not contain field '{}' but JSON does.", key)))?)
                        };
                        Ok((key, Value::try_deserialize_parsed(json_val, expected_field_type)?))
                    })
                    .collect();
                if let Some(tuple_type) = tuple_type {
                    TupleData::from_data_typed(deserialized_entries?, tuple_type)
                        .map(Value::from)
                } else {
                    TupleData::from_data(deserialized_entries?)
                        .map(Value::from)
                }
            },
            JSONParser::ContractPrincipal { type_n, issuer, name } => {
                if type_n != TYPE_CONTRACT_PRINCIPAL {
                    return Err(InterpreterError::DeserializeUnexpectedTypeField(type_n).into())
                }
                check_match!(expected_type, AtomTypeIdentifier::PrincipalType)?;
                Ok(Value::from(
                    if issuer == ":none:" {
                        PrincipalData::ContractPrincipal(name)
                    } else {
                        PrincipalData::QualifiedContractPrincipal {
                            sender: PrincipalData::parse_standard_principal(&issuer)?,
                            name }
                    }
                ))
            }
        }
    }

    pub fn try_deserialize(json: &str, expected: &TypeSignature) -> InterpreterResult<Value> {
        let data: JSONParser = serde_json::from_str(json)?;
        Value::try_deserialize_parsed(data, Some(expected))
    }

    pub fn try_deserialize_untyped(json: &str) -> InterpreterResult<Value> {
        let data: JSONParser = serde_json::from_str(json)?;
        Value::try_deserialize_parsed(data, None)
    }
}

impl Value {
    pub fn deserialize(json: &str, expected: &TypeSignature) -> Self {
        Value::try_deserialize(json, expected)
            .expect("ERROR: Failed to parse Clarity JSON")
    }
}


#[cfg(test)]
mod tests {
    use vm::database::ClaritySerializable;
    use vm::errors::Error;
    use super::super::*;

    #[test]
    fn test_lists() {
        let serialized_0 = r#"
            { "type": "list",
              "entries": [ { "type": "list", "entries": [ { "type": "i128", "value": "1" }, 
                                                          { "type": "i128", "value": "2" }, 
                                                          { "type": "i128", "value": "3" } ] },
                           { "type": "list", "entries": [ { "type": "i128", "value": "1" }, 
                                                          { "type": "i128", "value": "2" }, 
                                                          { "type": "i128", "value": "3" } ] } ] }"#;

        let serialized_1 = r#"
            { "type": "list",
              "entries": [ { "type": "list", "entries": [ { "type": "i128", "value": "1" }, 
                                                          { "type": "i128", "value": "3" } ] },
                           { "type": "list", "entries": [ { "type": "i128", "value": "1" }, 
                                                          { "type": "i128", "value": "3" } ] },
                           { "type": "list", "entries": [ { "type": "i128", "value": "1" }, 
                                                          { "type": "i128", "value": "3" } ] } ] }"#;

        // Should be legal!
        Value::try_deserialize(
            serialized_0, &TypeSignature::List(
                ListTypeData { max_len: 3, dimension: 2, atomic_type: AtomTypeIdentifier::IntType })).unwrap();
        Value::try_deserialize(
            serialized_0, &TypeSignature::List(
                ListTypeData { max_len: 5, dimension: 2, atomic_type: AtomTypeIdentifier::IntType })).unwrap();

        assert_eq!(
            Value::try_deserialize(
                serialized_0, &TypeSignature::List(
                    ListTypeData { max_len: 3, dimension: 2, atomic_type: AtomTypeIdentifier::IntType })).unwrap(),
            Value::try_deserialize_untyped(serialized_0).unwrap());

        // Fail because the atomic type isn't correct
        //  leads to an unexpected attempt to deserialize an int as bool.
        assert_eq!(Value::try_deserialize(
            serialized_0, &TypeSignature::List(
                ListTypeData { max_len: 3, dimension: 2, atomic_type: AtomTypeIdentifier::BoolType })).unwrap_err(),
                   InterpreterError::DeserializeExpected(
                       TypeSignature::Atom(AtomTypeIdentifier::BoolType)).into());
        
        // Fail because the max_len isn't enough for the sublists
        assert_eq!(Value::try_deserialize(
            serialized_0, &TypeSignature::List(
                ListTypeData { max_len: 2, dimension: 2, atomic_type: AtomTypeIdentifier::IntType })).unwrap_err(),
                   InterpreterError::FailureConstructingListWithType.into());
        
        // Fail because the max_len isn't enough for the outer-list
        assert_eq!(Value::try_deserialize(
            serialized_1, &TypeSignature::List(
                ListTypeData { max_len: 2, dimension: 2, atomic_type: AtomTypeIdentifier::IntType })).unwrap_err(),
                   InterpreterError::FailureConstructingListWithType.into());
        
        // Fail because dimension is bad
        //  leads to an unexpected attempt to deserialize an int as list.
        assert!(match Value::try_deserialize(
            serialized_1, &TypeSignature::List(
                ListTypeData { max_len: 3, dimension: 3, atomic_type: AtomTypeIdentifier::IntType })).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });
        
        // Fail because we look like a list but the "TYPE" field is wrong
        assert!(match Value::try_deserialize(
            r#"{ "type": "listtt", "entries": []}"#,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize_untyped(
            r#"{ "type": "listtt", "entries": []}"#).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        // Fail because we look like a list but the expected type is not a list
        assert!(match Value::try_deserialize(
            r#"{ "type": "list", "entries": []}"#,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });
    }

    #[test]
    fn test_bools() {
        assert_eq!(Value::Bool(false).serialize(), r#"{ "type": "bool", "value": false }"#);
        assert_eq!(Value::Bool(true).serialize(), r#"{ "type": "bool", "value": true }"#);

        assert!(match Value::try_deserialize(
            r#"{ "type": "bol", "value": false}"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BoolType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize_untyped(
            r#"{ "type": "bol", "value": false}"#).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            r#"{ "type": "bool", "value": false}"#,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });

        assert_eq!(
            Value::try_deserialize(
                r#"{ "type": "bool", "value": false}"#,
                &TypeSignature::Atom(AtomTypeIdentifier::BoolType)).unwrap(),
            Value::try_deserialize_untyped(
                r#"{ "type": "bool", "value": false}"#).unwrap());
    }

    #[test]
    fn test_ints() {
        assert_eq!(Value::Int(-1).serialize(), r#"{ "type": "i128", "value": "-1" }"#);
        assert_eq!(Value::Int(15).serialize(), r#"{ "type": "i128", "value": "f" }"#);

        assert!(match Value::try_deserialize(
            r#"{ "type": "i125", "value": "-f"}"#,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize_untyped(
            r#"{ "type": "i125", "value": "-f"}"#).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            r#"{ "type": "i128", "value": "-f"}"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BoolType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            r#"{ "type": "i128", "value": "-xf"}"#,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Runtime(RuntimeErrorType::ParseError(_),_) => true,
            _ => false
        });

        assert!(match Value::try_deserialize_untyped(
            r#"{ "type": "i128", "value": "-xf"}"#).unwrap_err() {
            Error::Runtime(RuntimeErrorType::ParseError(_),_) => true,
            _ => false
        });

        assert_eq!(
            Value::try_deserialize(
                r#"{ "type": "i128", "value": "-1"}"#,
                &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap(),
            Value::Int(-1));
        assert_eq!(
            Value::try_deserialize_untyped(
                r#"{ "type": "i128", "value": "-1"}"#).unwrap(),
            Value::Int(-1));
    }

    #[test]
    fn test_opts() {
        let none =  r#"{ "type": "none" }"#;
        let some_int = r#"{ "type": "some", "value": { "type": "i128", "value": "f" } }"#;

        assert_eq!(Value::some(Value::Int(15)).serialize(), some_int);
        assert_eq!(Value::none().serialize(), none);

        assert!(match Value::try_deserialize(
            none,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            some_int,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            some_int,
            &TypeSignature::List(
                ListTypeData { max_len: 2, dimension: 2, atomic_type: AtomTypeIdentifier::IntType })).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            r#"{ "type": "s0me", "value": { "type": "i128", "value": "f" } }"#,
            &TypeSignature::new_option(AtomTypeIdentifier::IntType.into())).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize_untyped(
            r#"{ "type": "s0me", "value": { "type": "i128", "value": "f" } }"#).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize_untyped(
            r#"{ "type": "n0ne" }"#).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });


        assert!(match Value::try_deserialize(
            r#"{ "type": "n0ne" }"#,
            &TypeSignature::new_option(AtomTypeIdentifier::IntType.into())).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
            _ => false
        });

        // Bad expected _contained_ type
        assert!(match Value::try_deserialize(
            some_int,
            &TypeSignature::new_option(AtomTypeIdentifier::BoolType.into())).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(x)) => x == AtomTypeIdentifier::BoolType.into(),
            _ => false
        });

        assert_eq!(
            Value::try_deserialize(
                some_int,
                &TypeSignature::new_option(AtomTypeIdentifier::IntType.into())).unwrap(),
            Value::some(Value::Int(15)));
        assert_eq!(
            Value::try_deserialize(
                none,
                &TypeSignature::new_option(AtomTypeIdentifier::IntType.into())).unwrap(),
            Value::none());
        assert_eq!(
            Value::try_deserialize(
                some_int,
                &TypeSignature::new_option(AtomTypeIdentifier::IntType.into())).unwrap(),
            Value::some(Value::Int(15)));
        assert_eq!(
            Value::try_deserialize_untyped(some_int).unwrap(),
            Value::some(Value::Int(15)));
        assert_eq!(
            Value::try_deserialize_untyped(none).unwrap(),
            Value::none());

    }

    #[test]
    fn test_resp() {
        let ok_int =  r#"{ "type": "ok", "value": { "type": "i128", "value": "f" } }"#;
        let err_int = r#"{ "type": "err", "value": { "type": "i128", "value": "f" } }"#;

        assert_eq!(Value::okay(Value::Int(15)).serialize(), ok_int);
        assert_eq!(Value::error(Value::Int(15)).serialize(), err_int);

        // Bad expected types.

        assert!(match Value::try_deserialize(
            err_int,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            ok_int,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
            _ => false
        });

        // Bad expected _contained_ type.

        assert!(match Value::try_deserialize(
            ok_int,
            &TypeSignature::new_response(AtomTypeIdentifier::BoolType.into(), AtomTypeIdentifier::IntType.into())).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(x)) => x == AtomTypeIdentifier::BoolType.into(),
            _ => false
        });

        assert!(match Value::try_deserialize(
            err_int,
            &TypeSignature::new_response(AtomTypeIdentifier::IntType.into(), AtomTypeIdentifier::BoolType.into())).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(x)) => x == AtomTypeIdentifier::BoolType.into(),
            _ => false
        });

        assert_eq!(
            Value::try_deserialize(
                ok_int,
                &TypeSignature::new_response(AtomTypeIdentifier::IntType.into(), AtomTypeIdentifier::IntType.into())).unwrap(),
            Value::okay(Value::Int(15)));
        assert_eq!(
            Value::try_deserialize(
                err_int,
                &TypeSignature::new_response(AtomTypeIdentifier::IntType.into(), AtomTypeIdentifier::IntType.into())).unwrap(),
            Value::error(Value::Int(15)));
        assert_eq!(
            Value::try_deserialize_untyped(ok_int).unwrap(),
            Value::okay(Value::Int(15)));
        assert_eq!(
            Value::try_deserialize_untyped(err_int).unwrap(),
            Value::error(Value::Int(15)));

    }

    #[test]
    fn test_buffs() {
        assert_eq!(Value::buff_from(vec![0,0,0,0]).unwrap().serialize(), 
                   r#"{ "type": "buff", "value": "00000000" }"#);
        assert_eq!(Value::buff_from(vec![0xde,0xad,0xbe,0xef]).unwrap().serialize(), 
                   r#"{ "type": "buff", "value": "deadbeef" }"#);
        assert_eq!(Value::buff_from(vec![0,0xde,0xad,0xbe,0xef,0]).unwrap().serialize(),
                   r#"{ "type": "buff", "value": "00deadbeef00" }"#);
        
        assert!(match Value::try_deserialize(
            r#"{ "type": "buff", "value": "00deadbeef00" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BoolType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
             _ => false
        });

        // fail because we expect a shorter buffer
        assert!(match Value::try_deserialize(
            r#"{ "type": "buff", "value": "00deadbeef00" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BufferType(4))).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
             _ => false
        });
        
        // fail because its a bad hex-string
        assert!(match Value::try_deserialize(
            r#"{ "type": "buff", "value": "00deadbeef0" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BufferType(6))).unwrap_err() {
            Error::Runtime(RuntimeErrorType::ParseError(_),_) => true,
            _ => false
        });

        assert_eq!(
            Value::try_deserialize(
            r#"{ "type": "buff", "value": "00deadbeef00" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BufferType(6))).unwrap(),
            Value::buff_from(vec![0,0xde,0xad,0xbe,0xef,0]).unwrap());
        assert_eq!(
            Value::try_deserialize_untyped(
                r#"{ "type": "buff", "value": "00deadbeef00" }"#).unwrap(),
            Value::buff_from(vec![0,0xde,0xad,0xbe,0xef,0]).unwrap());
    }

    #[test]
    fn test_tuples() {
        let t_1 = Value::from(TupleData::from_data(vec![
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(1))]).unwrap());
        let t_0 = Value::from(TupleData::from_data(vec![
            ("b".to_string(), Value::Int(1)),
            ("a".to_string(), Value::Int(1))]).unwrap());
        let t_2 = Value::from(TupleData::from_data(vec![
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Bool(true))]).unwrap());
        let t_3 = Value::from(TupleData::from_data(vec![
            ("a".to_string(), Value::Int(1))]).unwrap());
        assert_eq!(t_0.serialize(), r#"{ "type": "tuple", "entries": { "a": { "type": "i128", "value": "1" }, "b": { "type": "i128", "value": "1" } } }"#);
        assert_eq!(t_1.serialize(), r#"{ "type": "tuple", "entries": { "a": { "type": "i128", "value": "1" }, "b": { "type": "i128", "value": "1" } } }"#);
        assert_eq!(t_2.serialize(), r#"{ "type": "tuple", "entries": { "a": { "type": "i128", "value": "1" }, "b": { "type": "bool", "value": true } } }"#);
        assert_eq!(t_3.serialize(), r#"{ "type": "tuple", "entries": { "a": { "type": "i128", "value": "1" } } }"#);

        // JSON struct looks like tuple, but has bad type field.
        assert!(match Value::try_deserialize(
            r#"{ "type": "tople", "entries": {} }"#,
            &TypeSignature::type_of(&t_1)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
             _ => false
        });

        // bad expected type
        assert!(match Value::try_deserialize(
            r#"{ "type": "tuple", "entries": { "a": { "type": "i128", "value": "1" } } }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::IntType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
             _ => false
        });

        // non-existent field ("b" is in serialization, but not in expected type)
        assert!(match Value::try_deserialize(&t_0.serialize(), &TypeSignature::type_of(&t_3)).unwrap_err() {
            Error::Runtime(RuntimeErrorType::ParseError(_), _) => true,
             _ => false
        });

        // bad field type ("b" is int in serialization, but bool in expected type)
        assert!(match Value::try_deserialize(&t_0.serialize(), &TypeSignature::type_of(&t_2)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(x)) => x == AtomTypeIdentifier::BoolType.into(),
             _ => false
        });


        assert_eq!(&t_0, &t_1);
        assert_eq!(&Value::try_deserialize(&t_0.serialize(), &TypeSignature::type_of(&t_1)).unwrap(), &t_0);
        assert_eq!(&Value::try_deserialize(&t_0.serialize(), &TypeSignature::type_of(&t_0)).unwrap(), &t_0);
        assert_eq!(&Value::try_deserialize(&t_1.serialize(), &TypeSignature::type_of(&t_1)).unwrap(), &t_0);
        assert_eq!(&Value::try_deserialize(&t_1.serialize(), &TypeSignature::type_of(&t_0)).unwrap(), &t_0);
        assert_eq!(&Value::try_deserialize(&t_2.serialize(), &TypeSignature::type_of(&t_2)).unwrap(), &t_2);
        assert_eq!(&Value::try_deserialize(&t_3.serialize(), &TypeSignature::type_of(&t_3)).unwrap(), &t_3);
        assert_eq!(Value::try_deserialize(&t_3.serialize(), &TypeSignature::type_of(&t_3)).unwrap(), 
                   Value::try_deserialize_untyped(&t_3.serialize()).unwrap());
        assert_eq!(Value::try_deserialize(&t_2.serialize(), &TypeSignature::type_of(&t_2)).unwrap(), 
                   Value::try_deserialize_untyped(&t_2.serialize()).unwrap());
        assert_eq!(Value::try_deserialize(&t_1.serialize(), &TypeSignature::type_of(&t_1)).unwrap(), 
                   Value::try_deserialize_untyped(&t_1.serialize()).unwrap());
        assert_eq!(Value::try_deserialize(&t_0.serialize(), &TypeSignature::type_of(&t_0)).unwrap(), 
                   Value::try_deserialize_untyped(&t_0.serialize()).unwrap());
    }

    #[test]
    fn test_principals() {
        let standard_p = Value::from(PrincipalData::parse_standard_principal("SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G").unwrap());
        let contract_p1 = Value::from(PrincipalData::ContractPrincipal("foo".into()));
        let contract_p2 = Value::from(PrincipalData::QualifiedContractPrincipal{
            sender: PrincipalData::parse_standard_principal("SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G").unwrap(),
            name: "foo".into()});

        assert_eq!(standard_p.serialize(), r#"{ "type": "principal", "value": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G" }"#);
        assert_eq!(contract_p1.serialize(), r#"{ "type": "contract_principal", "issuer": ":none:", "name": "foo" }"#);
        assert_eq!(contract_p2.serialize(), r#"{ "type": "contract_principal", "issuer": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G", "name": "foo" }"#);
        
        assert!(match Value::try_deserialize(
            r#"{ "type": "principal", "value": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BoolType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
             _ => false
        });

        // fail because it looks like a contract principal, but has the wrong type field.
        assert!(match Value::try_deserialize(
            r#"{ "type": "contract__principal", "issuer": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G", "name": "foo" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::PrincipalType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeUnexpectedTypeField(_)) => true,
             _ => false
        });

        // fail because of expected type mismatch
        assert!(match Value::try_deserialize(
            r#"{ "type": "contract_principal", "issuer": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G", "name": "foo" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::BoolType)).unwrap_err() {
            Error::Interpreter(InterpreterError::DeserializeExpected(_)) => true,
             _ => false
        });

        // fail because its a bad address
        assert!(match Value::try_deserialize(
            r#"{ "type": "principal", "value": "SM2J6ZY48GV1EZ5V2V5RB9MP63SW86PYKKQVX8X0G" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::PrincipalType)).unwrap_err() {
            Error::Runtime(RuntimeErrorType::ParseError(_),_) => true,
            _ => false
        });

        assert!(match Value::try_deserialize(
            r#"{ "type": "contract_principal", "issuer": "SM2J6ZY48GV1EZ5V2V5RB9MP62SW86PYKKQVX8X0G", "name": "foo" }"#,
            &TypeSignature::Atom(AtomTypeIdentifier::PrincipalType)).unwrap_err() {
            Error::Runtime(RuntimeErrorType::ParseError(_),_) => true,
             _ => false
        });

        assert_eq!(
            &(Value::try_deserialize(
                r#"{ "type": "principal", "value": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G" }"#,
                &TypeSignature::Atom(AtomTypeIdentifier::PrincipalType)).unwrap()),
            &standard_p);

        assert_eq!(
            &(Value::try_deserialize(
                r#"{ "type": "contract_principal", "issuer": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G", "name": "foo" }"#,
                &TypeSignature::Atom(AtomTypeIdentifier::PrincipalType)).unwrap()),
            &contract_p2);

        assert_eq!(
            &(Value::try_deserialize(
                r#"{ "type": "contract_principal", "issuer": ":none:", "name": "foo" }"#,
                &TypeSignature::Atom(AtomTypeIdentifier::PrincipalType)).unwrap()),
            &contract_p1);

        assert_eq!(
            Value::try_deserialize_untyped(
                r#"{ "type": "principal", "value": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G" }"#).unwrap(),
            standard_p);

        assert_eq!(
            Value::try_deserialize_untyped(
                r#"{ "type": "contract_principal", "issuer": "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G", "name": "foo" }"#).unwrap(),
            contract_p2);

        assert_eq!(
            Value::try_deserialize_untyped(
                r#"{ "type": "contract_principal", "issuer": ":none:", "name": "foo" }"#).unwrap(),
            contract_p1);
    }

}