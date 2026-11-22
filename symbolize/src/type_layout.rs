//! Physical type layouts read from an explicitly identified local object.

use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use gimli::{
    AttributeValue, DwTag, Dwarf, EndianSlice, Reader, ReaderOffset, Relocate, RelocateReader,
    RunTimeEndian, Unit, UnitOffset,
};
use object::{Object, ObjectSection, RelocationMap};

/// One object and the DWARF layouts it actually contains.
#[derive(Clone, Debug)]
pub struct ObjectLayouts {
    /// Object inspected, without associating it with a different recording.
    pub path: PathBuf,
    /// Target architecture recorded in the object header.
    pub architecture: String,
    /// ELF build ID, when present.
    pub build_id: Option<String>,
    /// Complete sized structures, classes, and unions found in debug information.
    pub types: Vec<TypeLayout>,
}

/// One build-specific physical type layout.
#[derive(Clone, Debug)]
pub struct TypeLayout {
    /// Qualified type name.
    pub name: String,
    /// Declaring source file, when retained.
    pub source: Option<PathBuf>,
    /// Source language named by its compilation unit.
    pub language: String,
    /// Object containing this definition.
    pub module: PathBuf,
    /// Total size in bytes.
    pub size: u64,
    /// Explicit DW_AT_alignment; absent alignment is never inferred.
    pub alignment: Option<u64>,
    /// Direct physical members in declaration order.
    pub fields: Vec<FieldLayout>,
}

/// A direct member of a structure, class, or union.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldLayout {
    /// Member name, or an explicit anonymous-member label.
    pub name: String,
    /// Type name resolved within the defining compilation unit.
    pub type_name: String,
    /// Constant byte offset, when DWARF supplies one.
    pub offset: Option<u64>,
    /// Member size in bytes, when resolvable.
    pub size: Option<u64>,
    /// Bitfield width; these fields cannot establish whole-byte padding.
    pub bit_size: Option<u64>,
}

impl TypeLayout {
    /// Padding ranges, only when every member has a known whole-byte layout.
    pub fn padding(&self) -> Option<Vec<(u64, u64)>> {
        let mut ranges = Vec::new();
        for field in &self.fields {
            if field.bit_size.is_some() {
                return None;
            }
            let start = field.offset?;
            let end = start.checked_add(field.size?)?;
            if end > self.size {
                return None;
            }
            ranges.push((start, end));
        }
        ranges.sort_unstable();
        let mut end = 0;
        let mut gaps = Vec::new();
        for (start, next) in ranges {
            if start > end {
                gaps.push((end, start - end));
            }
            end = end.max(next);
        }
        if end < self.size {
            gaps.push((end, self.size - end));
        }
        Some(gaps)
    }
}

/// Failure to read an object or its debug information.
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    /// Local object could not be read.
    #[error("could not read object: {0}")]
    Io(#[from] std::io::Error),
    /// Object, compressed section, or section relocation could not be read.
    #[error("could not read object sections: {0}")]
    Object(#[from] object::Error),
    /// DWARF was invalid.
    #[error("could not read type debug information: {0}")]
    Dwarf(#[from] gimli::Error),
}

#[derive(Clone, Copy, Debug)]
struct DwarfRelocations<'a>(&'a RelocationMap);

impl Relocate for DwarfRelocations<'_> {
    fn relocate_address(&self, offset: usize, value: u64) -> gimli::Result<u64> {
        Ok(self.0.relocate(offset.into_u64(), value))
    }

    fn relocate_offset(&self, offset: usize, value: usize) -> gimli::Result<usize> {
        usize::from_u64(self.0.relocate(offset.into_u64(), value.into_u64()))
    }
}

type DwarfReader<'a> = RelocateReader<EndianSlice<'a, RunTimeEndian>, DwarfRelocations<'a>>;

struct Node {
    tag: DwTag,
    name: Option<String>,
    qualified: String,
    size: Option<u64>,
    alignment: Option<u64>,
    ty: Option<UnitOffset<usize>>,
    offset: Option<u64>,
    bits: Option<u64>,
    count: Option<u64>,
    source: Option<PathBuf>,
    children: Vec<UnitOffset<usize>>,
}

/// Reads local DWARF without fetching debug data or substituting a sample build.
pub fn load(path: &Path) -> Result<ObjectLayouts, LayoutError> {
    let bytes = fs::read(path)?;
    let object = object::File::parse(bytes.as_slice())?;
    let endian = if object.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };
    let sections = gimli::DwarfSections::load(|id| match object.section_by_name(id.name()) {
        Some(section) => Ok((section.uncompressed_data()?, section.relocation_map()?)),
        None => Ok::<_, object::Error>((Cow::Borrowed(&[][..]), RelocationMap::default())),
    })?;
    let dwarf = sections.borrow(|(data, relocations)| {
        RelocateReader::new(
            EndianSlice::new(data, endian),
            DwarfRelocations(relocations),
        )
    });
    let mut layouts = ObjectLayouts {
        path: path.to_owned(),
        architecture: format!("{:?}", object.architecture()),
        build_id: object
            .build_id()?
            .map(|id| id.iter().map(|byte| format!("{byte:02x}")).collect()),
        types: Vec::new(),
    };
    let mut units = dwarf.units();
    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        load_unit(&dwarf, &unit, path, &mut layouts.types)?;
    }
    layouts
        .types
        .sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));
    layouts.types.dedup_by(|a, b| {
        a.name == b.name && a.source == b.source && a.size == b.size && a.fields == b.fields
    });
    Ok(layouts)
}

fn string(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    value: AttributeValue<DwarfReader<'_>>,
) -> Option<String> {
    Some(
        dwarf
            .attr_string(unit, value)
            .ok()?
            .to_string_lossy()
            .ok()?
            .into_owned(),
    )
}

fn source_path(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    index: u64,
) -> Option<PathBuf> {
    let header = unit.line_program.as_ref()?.header();
    let file = header.file(index)?;
    let name = string(dwarf, unit, file.path_name())?;
    let mut path = PathBuf::new();
    if let Some(directory) = unit
        .comp_dir
        .as_ref()
        .and_then(|directory| directory.to_string_lossy().ok())
    {
        path.push(directory.as_ref());
    }
    if let Some(directory) = file
        .directory(header)
        .and_then(|value| string(dwarf, unit, value))
    {
        path.push(directory);
    }
    path.push(name);
    Some(path)
}

fn load_unit(
    dwarf: &Dwarf<DwarfReader<'_>>,
    unit: &Unit<DwarfReader<'_>>,
    path: &Path,
    layouts: &mut Vec<TypeLayout>,
) -> Result<(), gimli::Error> {
    let mut nodes = HashMap::<UnitOffset<usize>, Node>::new();
    let mut ancestors = Vec::new();
    let mut entries = unit.entries();
    let mut depth = 0isize;
    let mut language = "Unknown".to_owned();
    while let Some((delta, entry)) = entries.next_dfs()? {
        depth += delta;
        ancestors.truncate(depth as usize);
        let mut node = Node {
            tag: entry.tag(),
            name: None,
            qualified: String::new(),
            size: None,
            alignment: None,
            ty: None,
            offset: None,
            bits: None,
            count: None,
            source: None,
            children: Vec::new(),
        };
        let mut attributes = entry.attrs();
        while let Some(attribute) = attributes.next()? {
            let value = attribute.value();
            match attribute.name() {
                gimli::DW_AT_name => node.name = string(dwarf, unit, value),
                gimli::DW_AT_byte_size => node.size = value.udata_value(),
                gimli::DW_AT_alignment => node.alignment = value.udata_value(),
                gimli::DW_AT_type => {
                    if let AttributeValue::UnitRef(offset) = value {
                        node.ty = Some(offset);
                    }
                }
                gimli::DW_AT_data_member_location => node.offset = value.udata_value(),
                gimli::DW_AT_bit_size => node.bits = value.udata_value(),
                gimli::DW_AT_data_bit_offset => {
                    node.offset = value.udata_value().map(|bits| bits / 8)
                }
                gimli::DW_AT_count => node.count = value.udata_value(),
                gimli::DW_AT_upper_bound => {
                    node.count = value.udata_value().and_then(|bound| bound.checked_add(1))
                }
                gimli::DW_AT_decl_file => {
                    if let AttributeValue::FileIndex(index) = value {
                        node.source = source_path(dwarf, unit, index);
                    }
                }
                gimli::DW_AT_language => {
                    if let AttributeValue::Language(value) = value {
                        language = match value {
                            gimli::DW_LANG_Rust => "Rust",
                            gimli::DW_LANG_C
                            | gimli::DW_LANG_C89
                            | gimli::DW_LANG_C99
                            | gimli::DW_LANG_C11 => "C",
                            gimli::DW_LANG_C_plus_plus
                            | gimli::DW_LANG_C_plus_plus_03
                            | gimli::DW_LANG_C_plus_plus_11
                            | gimli::DW_LANG_C_plus_plus_14 => "C++",
                            _ => "Unknown",
                        }
                        .to_owned();
                    }
                }
                _ => {}
            }
        }
        let mut names: Vec<&str> = ancestors
            .iter()
            .filter_map(|offset| nodes.get(offset))
            .filter(|parent| {
                matches!(
                    parent.tag,
                    gimli::DW_TAG_namespace
                        | gimli::DW_TAG_structure_type
                        | gimli::DW_TAG_class_type
                )
            })
            .filter_map(|parent| parent.name.as_deref())
            .collect();
        if let Some(name) = node.name.as_deref() {
            names.push(name);
        }
        node.qualified = names.join("::");
        if let Some(parent) = ancestors.last().and_then(|offset| nodes.get_mut(offset)) {
            parent.children.push(entry.offset());
        }
        ancestors.push(entry.offset());
        nodes.insert(entry.offset(), node);
    }
    for node in nodes.values().filter(|node| {
        matches!(
            node.tag,
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type | gimli::DW_TAG_union_type
        )
    }) {
        let Some(size) = node.size else {
            continue;
        };
        if node.name.is_none() {
            continue;
        }
        let mut fields = Vec::new();
        for member in node
            .children
            .iter()
            .filter_map(|offset| nodes.get(offset))
            .filter(|child| child.tag == gimli::DW_TAG_member)
        {
            let (type_name, field_size) = member
                .ty
                .map(|offset| {
                    resolve_type(
                        offset,
                        &nodes,
                        unit.encoding().address_size,
                        &mut Vec::new(),
                    )
                })
                .unwrap_or_else(|| ("Unavailable".to_owned(), None));
            fields.push(FieldLayout {
                name: member
                    .name
                    .clone()
                    .unwrap_or_else(|| "<anonymous>".to_owned()),
                type_name,
                offset: member
                    .offset
                    .or_else(|| (node.tag == gimli::DW_TAG_union_type).then_some(0)),
                size: member.size.or(field_size),
                bit_size: member.bits,
            });
        }
        if fields.is_empty() {
            continue;
        }
        layouts.push(TypeLayout {
            name: node.qualified.clone(),
            source: node.source.clone(),
            language: language.clone(),
            module: path.to_owned(),
            size,
            alignment: node.alignment,
            fields,
        });
    }
    Ok(())
}

fn resolve_type(
    offset: UnitOffset<usize>,
    nodes: &HashMap<UnitOffset<usize>, Node>,
    address_size: u8,
    visiting: &mut Vec<UnitOffset<usize>>,
) -> (String, Option<u64>) {
    let Some(node) = nodes.get(&offset) else {
        return ("Unavailable".to_owned(), None);
    };
    if visiting.contains(&offset) {
        return ("<recursive>".to_owned(), node.size);
    }
    visiting.push(offset);
    let pointer = matches!(
        node.tag,
        gimli::DW_TAG_pointer_type
            | gimli::DW_TAG_reference_type
            | gimli::DW_TAG_rvalue_reference_type
    );
    let result = if let Some(name) = node.name.as_ref() {
        let size = node.size.or_else(|| {
            node.ty
                .and_then(|child| resolve_type(child, nodes, address_size, visiting).1)
        });
        (
            if node.qualified.is_empty() {
                name.clone()
            } else {
                node.qualified.clone()
            },
            size,
        )
    } else if pointer {
        let name = node
            .ty
            .and_then(|child| nodes.get(&child))
            .and_then(|child| child.name.as_deref())
            .unwrap_or("void");
        (
            format!("{name}*"),
            node.size.or(Some(u64::from(address_size))),
        )
    } else if let Some(child) = node.ty {
        let (name, size) = resolve_type(child, nodes, address_size, visiting);
        if node.tag == gimli::DW_TAG_array_type {
            let count = node
                .children
                .iter()
                .filter_map(|offset| nodes.get(offset))
                .filter(|child| child.tag == gimli::DW_TAG_subrange_type)
                .try_fold(1u64, |count, child| count.checked_mul(child.count?));
            (
                format!(
                    "{name}[{}]",
                    count.map_or_else(|| "?".to_owned(), |count| count.to_string())
                ),
                node.size.or_else(|| size?.checked_mul(count?)),
            )
        } else {
            (name, node.size.or(size))
        }
    } else {
        ("Unavailable".to_owned(), node.size)
    };
    visiting.pop();
    result
}
