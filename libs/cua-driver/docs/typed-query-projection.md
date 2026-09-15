# Typed macOS query projection

The macOS AX walker selects query results from native node positions and recorded
parent positions, including display-only parents. It does not infer ancestry from
indentation: an omitted empty native container can leave a later sibling deeper
in the outline without making it a child of the preceding row.
The rendered outline and structured records use that same
selection. Display text is never parsed to recover target IDs. The full walked
node vector remains available for the existing cache and token registry.

Public queries keep matching logical nodes and their ancestors. Line breaks in
an outline value are displayed as escaped controls so they cannot create extra
tree rows. Structured values retain the original characters. Search still uses
the original node text, including values, descriptions and metadata.

On macOS, `get_window_state` accepts optional `query_context:true` with a
nonblank `query` and accessibility enabled. It retains each matched node's
collected descendants as well as the matches and ancestors. Omitted or false
preserves the ordinary query behavior. This uses the shared preorder selector
over the same bounded walk, with no additional AX walk or screenshot.

For example, a query for a message heading can retain its child body and links.
Display-only children remain in `tree_markdown`; structured `elements` still
contain only actionable nodes. Read the outline too when the requested message
text is display-only. Counts describe actionable records, not all outline rows.
Original values, URLs, indices, and snapshot tokens remain unchanged.

Context does not include sibling replies merely because they share an ancestor
with a match. If the query itself matches that ancestor, its collected subtree
is included. Unloaded messages and nodes beyond the traversal limits remain
unavailable. Warnings are preserved and `elements_complete` remains false.
Neither selecting a subtree nor finishing a bounded walk proves a complete
search domain, and a missing match is not proof of absence.

This adapter change is macOS-only. Windows UIA and Linux AT-SPI still use their
existing formatted-tree projection. Their handling of multiline text has not
been validated by this change, and they do not advertise `query_context`.
Clients must consult the live tool schema before using platform-specific options.
The common selector is reusable by those adapters;
no cross-platform behavior or desktop certification is claimed here.

This is a local candidate for an additive MCP option. Public adoption still
requires the repository's RFC and review process; no upstream acceptance is
implied by this implementation or its local test results.
