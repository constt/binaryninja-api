// Copyright 2021-2023 Vector 35 Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::die_handlers::*;
use crate::dwarfdebuginfo::{DebugInfoBuilder, TypeUID};
use crate::helpers::*;

use binaryninja::{
    rc::*,
    types::{
        MemberAccess, MemberScope, ReferenceType, StructureBuilder, StructureType, Type, TypeClass,
    },
};

use gimli::{constants, AttributeValue::UnitRef, DebuggingInformationEntry, Dwarf, Reader, Unit};

use std::ffi::CString;

pub(crate) fn parse_data_variable<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R>,
    debug_info_builder: &mut DebugInfoBuilder,
) {
    let full_name = debug_info_builder.get_name(unit, entry);
    let type_uid = get_type(dwarf, unit, entry, debug_info_builder);

    let address = if let Ok(Some(attr)) = entry.attr(constants::DW_AT_location) {
        get_expr_value(unit, attr)
    } else {
        None
    };

    if let (Some(address), Some(type_uid)) = (address, type_uid) {
        debug_info_builder.add_data_variable(address, full_name, type_uid);
    }
}

fn do_structure_parse<R: Reader<Offset = usize>>(
    structure_type: StructureType,
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R>,
    debug_info_builder: &mut DebugInfoBuilder,
) -> Option<usize> {
    // All struct, union, and class types will have:
    //   *DW_AT_name
    //   *DW_AT_byte_size or *DW_AT_bit_size
    //   *DW_AT_declaration
    //   *DW_AT_signature
    //   *DW_AT_specification
    //   ?DW_AT_abstract_origin
    //   ?DW_AT_accessibility
    //   ?DW_AT_allocated
    //   ?DW_AT_associated
    //   ?DW_AT_data_location
    //   ?DW_AT_description
    //   ?DW_AT_start_scope
    //   ?DW_AT_visibility
    //   * = Optional

    // Structure/Class/Union _Children_ consist of:
    //  Data members:
    //   DW_AT_type
    //   *DW_AT_name
    //   *DW_AT_accessibility (default private for classes, public for everything else)
    //   *DW_AT_mutable
    //   *DW_AT_data_member_location xor *DW_AT_data_bit_offset (otherwise assume zero) <- there are some deprecations for DWARF 4
    //   *DW_AT_byte_size xor DW_AT_bit_size, iff the storage size is different than it usually would be for the given member type
    //  Function members:
    //   *DW_AT_accessibility (default private for classes, public for everything else)
    //   *DW_AT_virtuality (assume false)
    //      If true: DW_AT_vtable_elem_location
    //   *DW_AT_explicit (assume false)
    //   *DW_AT_object_pointer (assume false; for non-static member function; references the formal parameter that has "DW_AT_artificial = true" and represents "self" or "this" (language specified))
    //   *DW_AT_specification
    //   * = Optional

    if let Ok(Some(_)) = entry.attr(constants::DW_AT_declaration) {
        return None;
    }

    let full_name = if get_name(dwarf, unit, entry).is_some() {
        debug_info_builder.get_name(unit, entry)
    } else {
        None
    };

    // Create structure with proper size
    let size = get_size_as_u64(entry).unwrap_or(0);
    let structure_builder: StructureBuilder = StructureBuilder::new();
    structure_builder
        .set_packed(true)
        .set_width(size)
        .set_structure_type(structure_type);

    // This reference type will be used by any children to grab while we're still building this type
    //  it will also be how any other types refer to this struct
    if let Some(full_name) = &full_name {
        debug_info_builder.add_type(
            get_uid(unit, entry),
            full_name.clone(),
            Type::named_type_from_type(
                full_name.clone(),
                &Type::structure(&structure_builder.finalize()),
            ),
            false,
        );
    }

    // Get all the children and populate
    let mut tree = unit.entries_tree(Some(entry.offset())).unwrap();
    let mut children = tree.root().unwrap().children();
    while let Ok(Some(child)) = children.next() {
        if child.entry().tag() == constants::DW_TAG_member {
            if let Some(child_type_id) = get_type(dwarf, unit, child.entry(), debug_info_builder) {
                if let Some((_, child_type)) = debug_info_builder.get_type(child_type_id) {
                    if let Some(child_name) = get_name(dwarf, unit, child.entry()).map_or(
                        if child_type.type_class() == TypeClass::StructureTypeClass {
                            Some(CString::new("").unwrap())
                        } else {
                            None
                        },
                        Some,
                    ) {
                        // TODO : support DW_AT_data_bit_offset for offset as well
                        if let Ok(Some(raw_struct_offset)) =
                            child.entry().attr(constants::DW_AT_data_member_location)
                        {
                            // TODO : Let this fail; don't unwrap_or_default get_expr_value
                            let struct_offset =
                                get_attr_as_u64(&raw_struct_offset).unwrap_or_else(|| {
                                    get_expr_value(unit, raw_struct_offset).unwrap_or_default()
                                });

                            structure_builder.insert(
                                child_type.as_ref(),
                                child_name,
                                struct_offset,
                                false,
                                MemberAccess::NoAccess, // TODO : Resolve actual scopes, if possible
                                MemberScope::NoScope,
                            );
                        } else {
                            structure_builder.append(
                                child_type.as_ref(),
                                child_name,
                                MemberAccess::NoAccess,
                                MemberScope::NoScope,
                            );
                        }
                    }
                }
            }
        }
    }

    let finalized_structure = Type::structure(&structure_builder.finalize());
    if let Some(full_name) = full_name {
        debug_info_builder.add_type(
            get_uid(unit, entry) + 1, // TODO : This is super broke (uid + 1 is not guaranteed to be unique)
            full_name,
            finalized_structure,
            true,
        );
    } else {
        debug_info_builder.add_type(
            get_uid(unit, entry),
            CString::new(format!("{}", finalized_structure)).unwrap(),
            finalized_structure,
            false, // Don't commit anonymous unions (because I think it'll break things)
        );
    }
    Some(get_uid(unit, entry))
}

// This function iterates up through the dependency references, adding all the types along the way until there are no more or stopping at the first one already tracked, then returns the UID of the type of the given DIE
// TODO : Add a fail_list of UnitOffsets that already haven't been able to be parsed as not to duplicate work
pub(crate) fn get_type<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R>,
    debug_info_builder: &mut DebugInfoBuilder,
) -> Option<TypeUID> {
    // If this node (and thus all its referenced nodes) has already been processed, just return the offset
    if debug_info_builder.contains_type(get_uid(unit, entry)) {
        return Some(get_uid(unit, entry));
    }

    // Don't parse types that are just declarations and not definitions
    if let Ok(Some(_)) = entry.attr(constants::DW_AT_declaration) {
        return None;
    }

    // Passing through typedef was necessary at one point, but it seems that parsing has gotten robust enough not to explode on this....leaving it here just in case though
    // // Do not trust typedefs; typedefs should be transparent; typedefs mask the base type they refer to, not other typedefs
    // // This is the DIE that contains the type of the current DIE
    // let entry_type = if entry.tag() == constants::DW_TAG_typedef {
    //     let mut typedef_type = entry.clone();
    //     while typedef_type.tag() == constants::DW_TAG_typedef {
    //         if let Ok(Some(UnitRef(typedef_type_offset))) =
    //             typedef_type.attr_value(constants::DW_AT_type)
    //         {
    //             typedef_type = unit.entry(typedef_type_offset).unwrap();
    //         } else {
    //             return None;
    //         }
    //     }
    //     get_type(dwarf, unit, &typedef_type, debug_info_builder)
    // } else
    let entry_type =
        if let Ok(Some(UnitRef(entry_type_offset))) = entry.attr_value(constants::DW_AT_type) {
            // This needs to recurse first (before the early return below) to ensure all sub-types have been parsed
            get_type(
                dwarf,
                unit,
                &unit.entry(entry_type_offset).unwrap(),
                debug_info_builder,
            )
        } else if let Ok(Some(UnitRef(entry_type_offset))) =
            entry.attr_value(constants::DW_AT_specification)
        {
            // This needs to recurse first (before the early return below) to ensure all sub-types have been parsed
            get_type(
                dwarf,
                unit,
                &unit.entry(entry_type_offset).unwrap(),
                debug_info_builder,
            )
        } else if let Ok(Some(UnitRef(entry_type_offset))) =
            entry.attr_value(constants::DW_AT_abstract_origin)
        {
            // This needs to recurse first (before the early return below) to ensure all sub-types have been parsed
            get_type(
                dwarf,
                unit,
                &unit.entry(entry_type_offset).unwrap(),
                debug_info_builder,
            )
        } else {
            None
        };

    // If this node (and thus all its referenced nodes) has already been processed, just return the offset
    // This check is not redundant because this type might have been processes in the recursive calls above
    if debug_info_builder.contains_type(get_uid(unit, entry)) {
        return Some(get_uid(unit, entry));
    }

    // Collect the required information to create a type and add it to the type map. Also, add the dependencies of this type to the type's typeinfo
    // Create the type, make a TypeInfo for it, and add it to the debug info
    let (type_def, mut commit): (Option<Ref<Type>>, bool) = match entry.tag() {
        constants::DW_TAG_base_type => (handle_base_type(dwarf, unit, entry), false),

        constants::DW_TAG_structure_type => {
            return do_structure_parse(
                StructureType::StructStructureType,
                dwarf,
                unit,
                entry,
                debug_info_builder,
            )
        }
        constants::DW_TAG_class_type => {
            return do_structure_parse(
                StructureType::ClassStructureType,
                dwarf,
                unit,
                entry,
                debug_info_builder,
            )
        }
        constants::DW_TAG_union_type => {
            return do_structure_parse(
                StructureType::UnionStructureType,
                dwarf,
                unit,
                entry,
                debug_info_builder,
            )
        }

        // Enum
        constants::DW_TAG_enumeration_type => (handle_enum(dwarf, unit, entry), true),

        // Basic types
        constants::DW_TAG_typedef => handle_typedef(
            debug_info_builder,
            entry_type,
            debug_info_builder.get_name(unit, entry).unwrap(),
        ),
        constants::DW_TAG_pointer_type => (
            handle_pointer(
                entry,
                debug_info_builder,
                entry_type,
                ReferenceType::PointerReferenceType,
            ),
            false,
        ),
        constants::DW_TAG_reference_type => (
            handle_pointer(
                entry,
                debug_info_builder,
                entry_type,
                ReferenceType::ReferenceReferenceType,
            ),
            false,
        ),
        constants::DW_TAG_rvalue_reference_type => (
            handle_pointer(
                entry,
                debug_info_builder,
                entry_type,
                ReferenceType::RValueReferenceType,
            ),
            false,
        ),
        constants::DW_TAG_array_type => (
            handle_array(unit, entry, debug_info_builder, entry_type),
            false,
        ),

        // Strange Types
        constants::DW_TAG_unspecified_type => (Some(Type::void()), false), // TODO : Maybe true here
        constants::DW_TAG_subroutine_type => (
            handle_function(dwarf, unit, entry, debug_info_builder, entry_type),
            false,
        ),

        // Weird types
        constants::DW_TAG_const_type => (handle_const(debug_info_builder, entry_type), false),
        constants::DW_TAG_volatile_type => (handle_volatile(debug_info_builder, entry_type), true), // TODO : Maybe false here

        // Pass-through everything else!
        _ => return entry_type,
    };

    // Wrap our resultant type in a TypeInfo so that the internal DebugInfo class can manage it
    if let Some(type_def) = type_def {
        let name = if get_name(dwarf, unit, entry).is_some() {
            debug_info_builder.get_name(unit, entry)
        } else {
            None
        }
        .unwrap_or_else(|| {
            commit = false;
            CString::new(format!("{}", type_def)).unwrap()
        });

        debug_info_builder.add_type(get_uid(unit, entry), name, type_def, commit);
        Some(get_uid(unit, entry))
    } else {
        None
    }
}
