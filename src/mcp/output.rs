use std::{collections::BTreeMap, io};

use rmcp::ErrorData as McpError;
use serde_json::{Value as JsonValue, json};

pub(super) const DEFAULT_LIMIT: u32 = 100;
pub(super) const MAX_LIMIT: u32 = 500;
pub(super) const MAX_BYTES: usize = 64 * 1024;

pub(super) fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

pub(super) fn validate_limit(limit: u32) -> Result<(), McpError> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(McpError::invalid_params(
            format!("`limit` must be between 1 and {MAX_LIMIT}"),
            None,
        ));
    }
    Ok(())
}

pub(super) struct Page {
    pub items: Vec<JsonValue>,
    pub metadata: JsonValue,
}

impl Page {
    pub fn text(&self, subject: &str) -> String {
        let mut text = format!(
            "Returned {} of {} {subject} at offset {}.",
            self.metadata["returned"],
            self.metadata["total"],
            self.metadata["offset"],
        );
        if self.metadata["truncated"] == true {
            text.push_str(" Result truncated.");
        }
        if let Some(next) = self.metadata.get("nextOffset") {
            text.push_str(&format!(
                " Continue with offset {next} and the same query arguments."
            ));
        }
        if let Some(omitted) = self.metadata["omitted"].as_array() {
            text.push_str(&format!(
                " Omitted {} oversized items; their indexes are in pagination.omitted.",
                omitted.len(),
            ));
        }
        text
    }
}

pub(super) fn paginate(
    items: Vec<JsonValue>,
    limit: u32,
    offset: u64,
    hierarchical: bool,
) -> Page {
    let items = if hierarchical {
        let mut flattened = Vec::new();
        flatten_symbols(items, None, &mut flattened);
        flattened
    } else {
        items
    };
    let total = items.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let mut next = start;
    let mut bytes = 2;
    let mut selected = Vec::new();
    let mut omitted = Vec::new();
    for (index, item) in items
        .into_iter()
        .enumerate()
        .skip(start)
        .take(limit as usize)
    {
        let mut size = ItemSize(0);
        if serde_json::to_writer(&mut size, &item).is_err() {
            omitted.push(index);
            next = index + 1;
            continue;
        }
        // Empty child arrays make this a conservative bound even after rebuilding the tree.
        let size = size.0 + usize::from(!selected.is_empty());
        if size > MAX_BYTES - bytes {
            break;
        }
        bytes += size;
        selected.push(item);
        next = index + 1;
    }
    let mut metadata = json!({
        "offset": offset,
        "limit": limit,
        "maxBytes": MAX_BYTES,
        "returned": selected.len(),
        "total": total,
        "truncated": next < total || !omitted.is_empty(),
    });
    if next < total {
        metadata["nextOffset"] = json!(next);
    }
    if !omitted.is_empty() {
        metadata["omitted"] = json!(omitted);
    }
    Page {
        items: if hierarchical {
            rebuild_symbols(selected)
        } else {
            selected
        },
        metadata,
    }
}

// Count escaped JSON bytes without allocating another copy of a potentially oversized item.
struct ItemSize(usize);

impl io::Write for ItemSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES - 2 - self.0 {
            return Err(io::Error::other("item exceeds output budget"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn flatten_symbols(
    symbols: Vec<JsonValue>,
    parent: Option<usize>,
    flattened: &mut Vec<JsonValue>,
) {
    for mut symbol in symbols {
        let children = symbol["children"]
            .take()
            .as_array_mut()
            .map(std::mem::take)
            .expect("normalized document symbols have child arrays");
        let index = flattened.len();
        symbol["index"] = json!(index);
        symbol["parentIndex"] = json!(parent);
        symbol["childCount"] = json!(children.len());
        symbol["children"] = json!([]);
        flattened.push(symbol);
        flatten_symbols(children, Some(index), flattened);
    }
}

fn rebuild_symbols(symbols: Vec<JsonValue>) -> Vec<JsonValue> {
    let mut nodes: BTreeMap<_, _> = symbols
        .into_iter()
        .map(|symbol| (symbol["index"].as_u64().unwrap(), symbol))
        .collect();
    let mut roots = Vec::new();
    while let Some((_, symbol)) = nodes.pop_last() {
        if let Some(parent) = symbol["parentIndex"]
            .as_u64()
            .and_then(|index| nodes.get_mut(&index))
        {
            parent["children"].as_array_mut().unwrap().insert(0, symbol);
        } else {
            roots.push(symbol);
        }
    }
    roots.reverse();
    roots
}

pub(super) fn limit_schema() -> JsonValue {
    json!({
        "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT,
        "description": "Maximum items examined for this page, counting every nested document symbol. A 64 KiB serialized item budget may shorten the page."
    })
}

pub(super) fn offset_schema() -> JsonValue {
    json!({
        "type": "integer", "minimum": 0, "maximum": u64::MAX, "default": 0,
        "description": "Zero-based result offset. Continue using pagination.nextOffset with the same query arguments. Each call reruns the query; changing files or server indexes can shift results."
    })
}

pub(super) fn pagination_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "offset": { "type": "integer", "minimum": 0 },
            "limit": limit_schema(),
            "maxBytes": { "const": MAX_BYTES, "description": "Maximum compact JSON bytes in the result array, excluding pagination and readiness metadata." },
            "returned": { "type": "integer", "minimum": 0, "maximum": MAX_LIMIT },
            "total": { "type": "integer", "minimum": 0, "description": "Total items in this query response, including nested symbols and oversized items." },
            "truncated": { "type": "boolean", "description": "True when more items follow this page or oversized items were omitted from it." },
            "nextOffset": { "type": "integer", "minimum": 1 },
            "omitted": {
                "type": "array", "maxItems": MAX_LIMIT,
                "items": { "type": "integer", "minimum": 0 },
                "description": "Result indexes of items too large to fit in a page on their own. Their contents are omitted, and continuation advances past them."
            }
        },
        "required": ["offset", "limit", "maxBytes", "returned", "total", "truncated"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pages_cover_results_once_and_stop_at_the_end() {
        let items: Vec<_> =
            (0..205).map(|index| json!({ "id": index })).collect();
        let mut received = Vec::new();
        let mut offset = 0;
        loop {
            let page = paginate(items.clone(), DEFAULT_LIMIT, offset, false);
            assert!(page.items.len() <= DEFAULT_LIMIT as usize);
            assert_eq!(page.metadata["total"], 205);
            assert_eq!(page.metadata["returned"], page.items.len());
            received.extend(page.items);
            let Some(next) = page.metadata["nextOffset"].as_u64() else {
                assert_eq!(page.metadata["truncated"], false);
                break;
            };
            assert_eq!(page.metadata["truncated"], true);
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(received, items);
        let beyond = paginate(items, 1, u64::MAX, false);
        assert!(beyond.items.is_empty());
        assert_eq!(beyond.metadata["offset"], u64::MAX);
        assert_eq!(beyond.metadata["truncated"], false);
    }

    #[test]
    fn nested_symbols_count_toward_the_limit_and_keep_parent_identity() {
        let items = vec![json!({
            "name": "parent", "index": 999, "parentIndex": 999,
            "children": [
                { "name": "child", "children": [
                    { "name": "grandchild", "children": [] }
                ] },
                { "name": "sibling", "children": [] }
            ]
        })];
        let first = paginate(items.clone(), 2, 0, true);
        assert_eq!(first.metadata["total"], 4);
        assert_eq!(first.metadata["returned"], 2);
        assert_eq!(first.metadata["nextOffset"], 2);
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0]["index"], 0);
        assert_eq!(first.items[0]["parentIndex"], JsonValue::Null);
        assert_eq!(first.items[0]["childCount"], 2);
        assert_eq!(first.items[0]["children"][0]["index"], 1);
        assert_eq!(first.items[0]["children"][0]["parentIndex"], 0);
        assert_eq!(first.items[0]["children"][0]["childCount"], 1);
        assert_eq!(first.items[0]["children"][0]["children"], json!([]));

        let second = paginate(items.clone(), 2, 2, true);
        assert_eq!(second.items.len(), 2);
        assert_eq!(second.items[0]["name"], "grandchild");
        assert_eq!(second.items[0]["parentIndex"], 1);
        assert_eq!(second.items[1]["name"], "sibling");
        assert_eq!(second.items[1]["parentIndex"], 0);
        assert_eq!(second.metadata["truncated"], false);

        let full = paginate(items, 10, 0, true);
        assert_eq!(full.items[0]["children"][0]["name"], "child");
        assert_eq!(full.items[0]["children"][1]["name"], "sibling");
        assert_eq!(
            full.items[0]["children"][0]["children"][0]["name"],
            "grandchild"
        );
    }

    #[test]
    fn byte_budget_counts_json_escaping_and_reports_oversized_items() {
        let large = json!({ "name": "\"🦀".repeat(MAX_BYTES / 10) });
        let huge = json!({ "data": "x".repeat(MAX_BYTES) });
        let items =
            vec![large.clone(), large.clone(), huge, json!({ "id": 3 })];
        let first = paginate(items.clone(), 100, 0, false);
        assert_eq!(first.items, vec![large.clone()]);
        assert_eq!(first.metadata["nextOffset"], 1);
        let second = paginate(items.clone(), 100, 1, false);
        assert_eq!(second.items, vec![large, json!({ "id": 3 })]);
        assert_eq!(second.metadata["omitted"], json!([2]));
        assert_eq!(second.metadata["truncated"], true);
        assert!(second.metadata.get("nextOffset").is_none());
        for page in [first, second] {
            assert!(
                serde_json::to_vec(&page.items).unwrap().len() <= MAX_BYTES
            );
            let text = page.text("symbols");
            assert!(text.len() < 300);
            assert!(!text.contains('🦀'));
        }
    }

    #[test]
    fn oversized_parents_do_not_hide_children_or_stall_continuation() {
        let items = vec![json!({
            "name": "x".repeat(MAX_BYTES),
            "children": [{ "name": "child", "children": [] }]
        })];
        let first = paginate(items.clone(), 1, 0, true);
        assert!(first.items.is_empty());
        assert_eq!(first.metadata["omitted"], json!([0]));
        assert_eq!(first.metadata["nextOffset"], 1);
        let second = paginate(items, 1, 1, true);
        assert_eq!(second.items[0]["name"], "child");
        assert_eq!(second.items[0]["parentIndex"], 0);
    }

    #[test]
    fn an_item_that_exactly_fits_the_byte_budget_is_returned() {
        let item = json!("x".repeat(MAX_BYTES - 4));
        let page = paginate(vec![item.clone()], 1, 0, false);
        assert_eq!(page.items, vec![item]);
        assert_eq!(page.metadata["truncated"], false);
        assert_eq!(serde_json::to_vec(&page.items).unwrap().len(), MAX_BYTES);
    }
}
