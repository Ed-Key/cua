# Typed macOS query projection

The macOS AX walker selects query results from native node positions and stored
hierarchy depths. The rendered outline and structured records use that same
selection. Display text is never parsed to recover target IDs. The full walked
node vector remains available for the existing cache and token registry.

Public queries keep matching logical nodes and their ancestors. Line breaks in
an outline value are displayed as escaped controls so they cannot create extra
tree rows. Structured values retain the original characters. Search still uses
the original node text, including values, descriptions and metadata.

The shared preorder selector also supports descendants for internal tests of
contextual reading. This does not enable a new public query mode. A public
context option needs an explicit contract decision, including what happens to
sibling replies, unloaded content and traversal limits. Neither selecting a
subtree nor finishing a bounded walk proves a complete search domain.

This adapter change is macOS-only. Windows UIA and Linux AT-SPI still use their
existing formatted-tree projection. Their handling of multiline text has not
been validated by this change. The common selector is reusable by those adapters;
no cross-platform behavior or desktop certification is claimed here.
