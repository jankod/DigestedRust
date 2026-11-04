use std::io::BufRead;

use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;

use crate::uniprot::division::{Division, get_division_id};

#[derive(Debug, Default)]
pub struct ProteinInfo {
    pub accession: Option<String>,
    pub protein_name: Option<String>,
    pub taxonomy_id: i32,
    pub division_id: i32,
    pub sequence: Option<String>,
}

pub fn parse_proteins<R, F>(reader_input: R, mut on_protein: F) -> anyhow::Result<u64>
where
    R: BufRead,
    F: FnMut(ProteinInfo, u64) -> anyhow::Result<()>,
{
    let mut reader = Reader::from_reader(reader_input);
    let config = reader.config_mut();
    config.trim_text(true);
    config.check_end_names = false;

    let mut buf = Vec::new();
    let mut entry_count: u64 = 0;

    let mut protein_info: Option<ProteinInfo> = None;
    let mut in_lineage = false;
    let mut is_organism = false;
    let mut in_recommended_name = false;
    let mut current_text = String::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => {
                let name = e.name();
                let name_bytes = name.as_ref();
                current_text.clear();

                match name_bytes {
                    b"entry" => {
                        protein_info = Some(ProteinInfo {
                            taxonomy_id: -1,
                            division_id: -1,
                            ..Default::default()
                        });
                    }
                    b"recommendedName" => {
                        in_recommended_name = true;
                    }
                    b"organism" => {
                        is_organism = true;
                    }
                    b"lineage" => {
                        in_lineage = true;
                    }
                    _ => {}
                }
            }
            Event::Empty(e) => {
                let name = e.name();
                let name_bytes = name.as_ref();

                match name_bytes {
                    b"dbReference" if is_organism && !in_lineage => {
                        if let Some(protein) = protein_info.as_mut() {
                            let mut attr_type = String::new();
                            let mut attr_id = String::new();

                            for attr in e.attributes() {
                                let attr = attr?;
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let value = String::from_utf8_lossy(&attr.value);

                                match key.as_ref() {
                                    "type" => attr_type = value.to_string(),
                                    "id" => attr_id = value.to_string(),
                                    _ => {}
                                }
                            }

                            if attr_type == "NCBI Taxonomy" && !attr_id.is_empty() {
                                protein.taxonomy_id = attr_id.parse().unwrap_or(-1);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(e) => {
                if protein_info.is_some() {
                    current_text.push_str(e.decode()?.as_ref());
                }
            }
            Event::GeneralRef(e) => {
                if protein_info.is_some() {
                    if let Some(ch) = e.resolve_char_ref()? {
                        current_text.push(ch);
                    } else {
                        let entity = e.decode()?;
                        if let Some(resolved) = resolve_predefined_entity(entity.as_ref()) {
                            current_text.push_str(resolved);
                        } else {
                            current_text.push('&');
                            current_text.push_str(entity.as_ref());
                            current_text.push(';');
                        }
                    }
                }
            }
            Event::End(e) => {
                let name = e.name();
                let name_bytes = name.as_ref();

                if let Some(protein) = protein_info.as_mut() {
                    let text = current_text.trim();
                    match name_bytes {
                        b"accession" if protein.accession.is_none() && !text.is_empty() => {
                            protein.accession = Some(text.to_string());
                        }
                        b"fullName" if in_recommended_name && !text.is_empty() => {
                            protein.protein_name = Some(text.to_string());
                        }
                        b"taxon" if in_lineage && !text.is_empty() => {
                            // Lineage is broad -> specific. Keep the last recognized non-unassigned
                            // division so we end up with the most specific supported class.
                            let candidate = get_division_id(text);
                            if candidate != Division::Unassigned.id() {
                                protein.division_id = candidate;
                            }
                        }
                        b"sequence" if !text.is_empty() => {
                            protein.sequence = Some(text.to_string());
                        }
                        _ => {}
                    }
                }

                match name_bytes {
                    b"entry" => {
                        if let Some(protein) = protein_info.take() {
                            let protein = ProteinInfo {
                                division_id: if protein.division_id == -1 {
                                    Division::Unassigned.id()
                                } else {
                                    protein.division_id
                                },
                                ..protein
                            };
                            entry_count += 1;
                            on_protein(protein, entry_count)?;
                        }
                    }
                    b"recommendedName" => {
                        in_recommended_name = false;
                    }
                    b"organism" => {
                        is_organism = false;
                    }
                    b"lineage" => {
                        in_lineage = false;
                    }
                    _ => {}
                }
                current_text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(entry_count)
}
