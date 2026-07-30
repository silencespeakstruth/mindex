; Vendored from the tree-sitter-c-sharp crate (v0.23.5, queries/tags.scm).
; One fix vs upstream: line 23 is a stray duplicate of the
; namespace_declaration pattern with a bare `@module` capture, which violates
; the tree-sitter-tags capture contract (@definition.*/@reference.*) and makes
; TagsConfiguration::new reject the whole query — the duplicate is dropped.
; Refresh (and re-apply) when bumping the crate.

(class_declaration name: (identifier) @name) @definition.class

(class_declaration (base_list (_) @name)) @reference.class

(interface_declaration name: (identifier) @name) @definition.interface

(interface_declaration (base_list (_) @name)) @reference.interface

(method_declaration name: (identifier) @name) @definition.method

(object_creation_expression type: (identifier) @name) @reference.class

(type_parameter_constraints_clause (identifier) @name) @reference.class

(type_parameter_constraint (type type: (identifier) @name)) @reference.class

(variable_declaration type: (identifier) @name) @reference.class

(invocation_expression function: (member_access_expression name: (identifier) @name)) @reference.send

(namespace_declaration name: (identifier) @name) @definition.module

