# Java cases and the jdtls tests they come from

Most cases here were derived from the handler tests of jdtls
(`org.eclipse.jdt.ls.tests/src/org/eclipse/jdt/ls/core/internal/handlers/`
in [eclipse-jdtls/eclipse.jdt.ls](https://github.com/eclipse-jdtls/eclipse.jdt.ls)).
A case file has no room for a comment — everything above `//= expected` is
the fixture — so this file says where each one comes from.

- A case listed with a jdtls test asks what that test asks, on sources small
  enough to read in the case. Where the jdtls test leans on the JDK or a
  library (`println`, `new RuntimeException`, `StringUtils`), the fixture
  declares a type of its own to play the part.
- A case listed without one asks the same jdtls handler about a kind of name
  its tests leave out; its answer follows from how that handler is written.
- The expectations are what jdtls answers, written without looking at what
  kmp-lsp answers. A case that fails is a difference from jdtls: fix the
  server, or show with a jdtls run that the expectation is wrong.

Notes on how an answer is written down:

- **references** always ask for the declaration too. No field has a
  `getX`/`setX` beside it: jdtls counts calls of those accessors as
  references to the field (`java.references.includeAccessors`), which is a
  setting, not a rule of the language.
- **document_highlight** follows jdtls's `Highlight.java`: besides the
  occurrences of a name, jdtls highlights where a thrown exception is
  thrown, where a method exits, the loop a `break` leaves and the methods
  implementing an interface.
- **implementation** asks every case of the hierarchy of jdtls's `hello`
  test project: `IFoo` is implemented by `Foo2`, which `Foo3` extends;
  `Foo` is extended by `FooChild`; nothing extends `AbstractFoo`.
- **rename** lists where each edit starts, on the line as it was before;
  the jdtls tests compare the sources after the rename. The new name is
  `renamed` (`Renamed` for a name that starts with a capital).
- **prepare_rename** and **rename** answer `(nothing)` where jdtls refuses
  with an error.
- **folding_range** uses the files of jdtls's `foldingRange` test project
  unchanged (tabs, blank lines and trailing spaces included: where a region
  ends depends on them). jdtls's tests count the regions of a file and name
  most of them; the few a test only counts are filled in by the same rules.
- **hover** is the signature line as jdtls words it, with the hovered name
  unqualified.
- **signature_help** cases that end in a syntax error do so on purpose, as
  the jdtls tests do: help is asked while a call is being typed.

## definition

jdtls: `NavigateToDefinitionHandlerTest`.

| case | jdtls test |
| --- | --- |
| `annotation` | — |
| `annotation_element` | — |
| `catch_parameter` | — |
| `constructor_call_is_the_constructor_with_those_arguments` | — |
| `declaration_is_itself` | — |
| `default_method_called_on_an_implementation` | — |
| `enum_constant` | — |
| `enum_constant_in_a_case_label` | — |
| `field_assigned_from_the_parameter_that_shadows_it` | — |
| `field_through_this_skips_the_local_of_the_same_name` | — |
| `fully_qualified_type` | — |
| `imported_type_in_its_import` | — |
| `inherited_method_is_in_the_superclass` | — |
| `lambda_parameter` | — |
| `last_field_of_a_field_chain` | — |
| `local_declared_with_var` | — |
| `local_variable` | — |
| `method_at_the_end_of_a_call_chain` | — |
| `method_called_after_a_cast` | — |
| `method_called_on_an_array_element` | — |
| `method_linked_from_javadoc` | — |
| `method_on_the_type_argument_of_a_generic_receiver` | — |
| `method_reference` | — |
| `middle_field_of_a_field_chain` | — |
| `nested_class_named_through_its_outer_class` | — |
| `outer_field_read_from_an_inner_class` | — |
| `overload_is_chosen_by_the_argument_type` | — |
| `overridden_method_is_the_override_of_the_receivers_type` | — |
| `parameter_assigned_to_the_field_it_shadows` | — |
| `pattern_variable` | — |
| `prefers_the_method_of_the_anonymous_class_over_the_enclosing_one` | `testMethodInAnonymousClass2` |
| `record_accessor_is_the_record_component` | — |
| `single_type_import_shadows_the_type_of_the_same_package` | — |
| `static_field_read_through_its_class` | — |
| `static_method_called_on_its_class` | — |
| `statically_imported_field` | — |
| `super_call_is_the_method_of_the_superclass` | — |
| `type_follows_the_import_not_the_same_name_elsewhere` | — |
| `type_imported_on_demand_not_the_same_name_elsewhere` | — |
| `type_in_a_declaration` | — |
| `type_parameter` | — |
| `variable_of_an_enhanced_for` | — |

## type_definition

jdtls: `NavigateToTypeDefinitionHandlerTest`.

| case | jdtls test |
| --- | --- |
| `enum_constant_is_the_enum` | — |
| `field_of_another_object` | `testExternalClassField` |
| `field_read_through_this` | `testClassField` |
| `generic_variable_is_the_generic_class` | — |
| `lambda_parameter_is_the_type_the_interface_gives_it` | — |
| `local_declared_with_var_is_the_inferred_class` | — |
| `local_is_the_class_it_is_declared_with` | `testLocalVariable` |
| `parameter` | — |
| `primitive_local_is_nothing` | — |
| `type_name_is_its_declaration` | — |

## implementation

jdtls: `ImplementationsHandlerTest`.

| case | jdtls test |
| --- | --- |
| `abstract_class_nothing_extends_is_the_class` | `testUnimplementedClassImplementation_includeDefinition` |
| `abstract_method_nothing_implements_is_the_method` | `testUnimplementedMethodImplementation_includeDefinition` |
| `call_on_a_class_includes_its_own_method` | `testMethodImplementation_includeDefinition` |
| `call_through_an_interface` | `testMethodInvocationImplementation` |
| `class_at_its_declaration_is_its_subclasses` | `testClassImplementation` |
| `class_where_it_is_used_includes_the_class` | `testClassImplementation_includeDefinition` |
| `interface_is_every_class_below_it` | `testInterfaceImplementation` |
| `interface_method_is_the_method_that_implements_it` | `testMethodImplementation` |
| `super_call_is_the_method_of_the_superclass` | `testMethodSuperInvocationImplementation` |
| `type_parameter_is_nothing` | `testInvalidElement` |

## references

jdtls: `ReferencesHandlerTest`.

| case | jdtls test |
| --- | --- |
| `class_everywhere_its_name_is_written` | — |
| `class_extended_in_another_file` | `testReference` |
| `class_nothing_uses_is_only_its_declaration` | `testPotentialMatch` (vscode-java#2227) |
| `enum_constant_with_its_case_label` | — |
| `field_asked_from_a_use` | — |
| `field_read_and_written` | — |
| `interface_method_called_through_the_interface` | — |
| `method_across_files_skips_the_same_name_in_another_class` | — |
| `method_with_a_method_reference` | — |
| `method_with_its_declaration` | `testDeclarationInReferences` (eclipse.jdt.ls#2148) |
| `overload_skips_the_other_overloads` | — |
| `static_method_called_with_and_without_its_class` | — |

## document_highlight

jdtls: `DocumentHighlightHandlerTest`.

| case | jdtls test |
| --- | --- |
| `class_is_every_use_of_its_name_in_the_file` | — |
| `field_skips_the_local_of_the_same_name` | `testDocumentHighlight_Occurrences` |
| `implemented_interface_is_the_methods_implementing_it` | `testDocumentHighlight_ImplementOccurrences` |
| `labelled_break_is_the_loop_it_leaves` | `testDocumentHighlight_BreakContinueTarget` |
| `local_stays_inside_its_method` | — |
| `method_is_its_declaration_and_calls_in_the_file` | — |
| `return_is_where_the_method_exits` | `testDocumentHighlight_MethodExits` |
| `return_type_is_where_the_method_exits` | `testDocumentHighlight_MethodExits` |
| `thrown_exception_is_where_it_is_thrown` | `testDocumentHighlight_ExceptionOccurences` |

## completion

jdtls: `CompletionHandlerTest`.

| case | jdtls test |
| --- | --- |
| `annotation_element_by_prefix` | `testCompletion_annotationParam` |
| `enum_constant_in_a_case_label` | — |
| `field_of_the_outer_class_inside_an_inner_class` | — |
| `locals_and_parameters_declared_before_the_cursor` | — |
| `method_of_the_superclass_after_super` | — |
| `only_the_public_members_of_a_class_of_another_package` | — |

## hover

jdtls: `HoverHandlerTest`.

| case | jdtls test |
| --- | --- |
| `blank_line_is_nothing` | `testEmptyHover` |
| `class_is_its_name` | `testHover` |
| `constant_shows_its_value` | — |
| `constructor_call_shows_the_constructor` | — |
| `enum_constant_is_its_name` | `testEnum` |
| `field_of_a_generic_type_shows_its_type_arguments` | — |
| `field_shows_its_type` | — |
| `generic_class_shows_its_type_parameters` | — |
| `generic_method_shows_its_type_parameters` | `testHoverTypeParameters` |
| `inherited_method_called_on_a_subclass` | — |
| `interface_method_called_through_the_interface` | — |
| `local_names_the_method_it_belongs_to` | — |
| `method_returning_a_type_parameter` | `testHoverTypeParameters` |
| `method_shows_its_return_type_and_parameters` | `testHoverMethod` |
| `method_shows_the_exceptions_it_throws` | — |
| `method_with_array_and_generic_parameters` | — |
| `nested_type_is_its_own_name` | `testHoverOnJava11var` |
| `parameter_names_the_method_it_belongs_to` | `testHoverVariable` |
| `static_method_does_not_show_static` | `testHoverMethod` |
| `string_constant_shows_its_value_quoted` | — |
| `type_parameter_is_its_name` | `testHoverTypeParameters` |
| `varargs_method` | — |

## signature_help

jdtls: `SignatureHelpHandlerTest`.

| case | jdtls test |
| --- | --- |
| `after_a_cast_is_the_method_of_the_cast_type` | `testSignatureHelp_differentDeclaringType` |
| `after_a_nested_call_is_the_outer_method` | `testSignatureHelp_nestedInvocation` |
| `after_a_string_holding_a_parenthesis` | `testSignatureHelp_stringLiteral` |
| `after_a_trailing_comma_is_the_next_parameter` | `testSignatureHelp_parameters` (eclipse.jdt.ls#1015) |
| `before_a_constructor_argument_is_the_enclosing_call` | `testSignatureHelp_enclosingMethods` |
| `class_without_a_constructor_is_the_default_one` | `testSignatureHelp_defaultConstructor` |
| `generic_class_skips_the_constructor_it_cannot_call` | `testSignatureHelp_erasureType` |
| `in_a_call_that_is_not_closed` | `testSignatureHelp_double` (eclipse.jdt.ls#1980) |
| `in_the_first_call_of_a_chain` | `testSignatureHelp_methodChain` |
| `in_the_last_call_of_a_chain` | `testSignatureHelp_methodChain` |
| `inside_a_constructor_argument_is_the_constructor` | `testSignatureHelp_enclosingMethods` |
| `inside_a_nested_call_is_the_inner_method` | `testSignatureHelp_nestedInvocation` |
| `inside_a_string_holding_a_comma` | `testSignatureHelp_comma`, `testSignatureHelp_bracket` |
| `inside_a_string_holding_a_parenthesis` | `testSignatureHelp_stringLiteral` |
| `inside_the_first_array_argument` | `testSignatureHelp_complexArgs` |
| `inside_the_second_array_argument` | `testSignatureHelp_complexArgs` |
| `later_varargs_argument_is_still_the_varargs_parameter` | `testSignatureHelp_varargs3` |
| `on_the_method_name_marks_no_parameter` | `testSignatureHelp_callFromMethodName` |
| `outside_a_call_is_nothing` | `testSignatureHelp_invalid` |
| `picks_the_overload_by_the_number_of_arguments` | `testSignatureHelp_parameterTypes` |
| `picks_the_overload_by_the_type_of_the_argument` | `testSignatureHelp_parameterObject` |
| `picks_the_overload_the_arguments_already_fit` | `testSignatureHelp_activeSignature` |
| `picks_the_overload_with_room_for_the_argument_being_typed` | `testSignatureHelp_multipleMethods` |
| `record_is_its_canonical_constructor` | `testSignatureHelp_record` |
| `shows_the_type_arguments_of_the_receiver` | `testSignatureHelp_genericTypes` |
| `single_method` | `testSignatureHelp_singleMethod` |
| `super_call_is_the_constructor_of_the_superclass` | `testSignatureHelp_superConstructorInvocation` |
| `this_call_at_its_second_argument` | `testSignatureHelp_constructorInvocation` |
| `this_call_is_the_other_constructor` | `testSignatureHelp_constructorInvocation` |
| `varargs_argument_ignores_parentheses_in_a_string` | `testSignatureHelp_varargs4` |

## prepare_rename

jdtls: `PrepareRenameHandlerTest`.

| case | jdtls test |
| --- | --- |
| `call_of_an_implicit_record_accessor` | `testRenameRecordImplicitAccessorCall` |
| `class` | `testRenameTypeWithResourceChanges` |
| `compact_constructor_parameter` | `testRenameRecordCompactConstructorParameter` |
| `explicit_record_accessor` | `testRenameRecordExplicitAccessor` |
| `field` | `testRenameField` |
| `lambda_parameter` | `testRenameLambdaParameter` |
| `local_variable` | `testRenameLocalVariable` |
| `method` | `testRenameMethod` |
| `parameter` | `testRenameParameter` |
| `parameter_described_in_javadoc` | `testRenameJavadoc` |
| `refuses_a_class_of_the_jdk` | `testRenameClassFile` |
| `refuses_a_fully_qualified_class_of_the_jdk` | `testRenameFQCNClassFile` |
| `refuses_a_package_declaration` | `testRenamePackage` |
| `refuses_a_package_of_the_jdk` | `testRenameBinaryPackage` |
| `refuses_an_import` | `testRenameImportDeclaration` |
| `refuses_the_middle_of_a_package_declaration` | `testRenameMiddleOfPackage` |
| `type_parameter_of_a_class` | `testRenameTypeParameter` |
| `type_parameter_of_a_method` | `testRenameTypeParameterInMethod` |

## rename

jdtls: `RenameHandlerTest`.

| case | jdtls test |
| --- | --- |
| `at_super_renames_the_superclass` | `testRenameSuperMethod` |
| `class_renames_its_constructor` | `testRenameTypeWithResourceChanges` |
| `class_renames_its_uses_in_another_file` | `testRenameType` |
| `constructor_of_a_record_leaves_the_component_of_the_same_name` | `testRenameConstructorOfRecordWhoseComponentIsNamedLikeIt` |
| `constructor_renames_its_class` | `testRenameConstructor` |
| `field` | `testRenameField` |
| `from_a_call_of_an_implicit_accessor_renames_the_record_component` | `testRenameRecordComponentFromImplicitAccessorCall` |
| `from_a_canonical_constructor_parameter_renames_the_record_component` | `testRenameRecordComponentFromCanonicalConstructorParameter` |
| `from_a_compact_constructor_parameter_renames_the_record_component` | `testRenameRecordComponentFromCompactConstructorParameter` |
| `from_an_explicit_accessor_renames_the_record_component` | `testRenameRecordComponentFromExplicitAccessor` |
| `implementation_renames_the_interface_method` | `testRenameInterfaceMethod` |
| `local_variable_leaves_the_same_name_in_another_method` | `testRenameLocalVariable` |
| `method` | `testRenameMethod` |
| `method_from_a_call_in_another_file` | `testRenameMultipleFiles` |
| `method_parameter_named_like_a_record_component_leaves_the_component` | `testRenameParameterOfOrdinaryMethodNamedLikeRecordComponent` |
| `override_renames_every_method_tied_to_it_by_a_shared_subclass` | `testRenameOverrideMethodComplex` |
| `override_renames_the_method_it_overrides` | `testRenameOverrideMethodSimple` |
| `parameter_leaves_the_same_name_in_another_method` | `testRenameParameter` |
| `parameter_renames_its_javadoc_tag` | `testRenameJavadoc` |
| `record_component_renames_it_in_the_compact_constructor` | `testRenameRecordComponentWithCompactConstructorBody` |
| `record_component_renames_its_accessor_calls` | `testRenameRecordField` (vscode-java#2433) |
| `record_component_renames_its_explicit_accessor` | `testRenameRecordComponentWithExplicitAccessor` |
| `refuses_a_method_of_the_jdk` | `testRenameSystemLibrary` |
| `type_parameter_of_a_class` | `testRenameTypeParameter` |
| `type_parameter_of_a_method_leaves_the_one_of_the_class` | `testRenameTypeParameterInMethod` |

## document_symbol

jdtls: `DocumentSymbolHandlerTest`.

| case | jdtls test |
| --- | --- |
| `names_generic_declarations_without_their_type_parameters` | — |
| `outlines_an_enum_with_members` | `testDecompiledSource` |
| `outlines_an_interface` | — |
| `outlines_every_top_level_type_and_nesting_level` | — |
| `outlines_nested_types` | `testTypes`, `testTypes_hierarchical` |
| `tells_a_constant_from_a_field` | — |

## folding_range

jdtls: `FoldingRangeHandlerTest`.

| case | jdtls test |
| --- | --- |
| `file_that_does_not_scan` | `testInvalidInput` |
| `folds_comments_imports_and_bodies` | `testTypes` |
| `folds_nested_switches_and_braces_on_their_own_line` | `testNestedSwitchFoldingRanges`, `testCurlyBracesOwnLine` (eclipse.jdt.ls#2865) |
| `folds_region_comments` | `testRegionFoldingRanges` |
| `folds_statements` | `testStatementFoldingRanges` |
| `folds_static_initializers` | `testStaticBlockFoldingRange` |
| `unbalanced_braces` | `testErrorTypes` |

## workspace_symbol

jdtls: `WorkspaceSymbolHandlerTest`.

| case | jdtls test |
| --- | --- |
| `finds_interfaces_enums_and_nested_types` | — |
| `finds_the_same_name_in_two_packages` | `testProjectSearch` |
| `matches_a_lower_case_query_as_a_prefix` | `testSearchQualifiedTypeNoWildcards` |
| `matches_camel_case_parts_anywhere_in_the_name` | `testCamelCaseFuzzySearch` |
| `matches_capitals_as_camel_case` | `testCamelCaseSearch` |
| `nothing_for_a_name_no_type_has` | `testSearchWithEmptyResults` |
