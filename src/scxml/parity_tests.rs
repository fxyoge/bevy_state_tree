use crate::definition::assert_definition_semantics_eq;
use crate::{
    BuildError, StateTreeAction, StateTreeBuilder, StateTreeData, StateTreeDefinition,
    StateTreeEffectArgument, StateTreeTransition,
};

macro_rules! definition_parity_test {
    ($test:ident, $fixture:literal, $builder:path) => {
        #[test]
        fn $test() {
            let source = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/builder_parity/",
                $fixture,
                ".scxml"
            ));
            let document = super::parse(concat!($fixture, ".scxml"), source).unwrap();
            let from_scxml = super::compile(&document).unwrap();
            let from_builder = $builder().unwrap();

            assert_definition_semantics_eq(&from_scxml, &from_builder);
        }
    };
}

definition_parity_test!(hierarchy_parity, "hierarchy", hierarchy_definition);
definition_parity_test!(transition_parity, "transitions", transition_definition);
definition_parity_test!(action_parity, "actions", action_definition);
definition_parity_test!(minimal_luau_parity, "minimal_luau", minimal_luau_definition);

fn hierarchy_definition() -> Result<StateTreeDefinition, BuildError> {
    let mut builder = StateTreeBuilder::new("hierarchy");
    let root = builder.compound_root("#document")?;
    let parent = builder.compound("parent", root)?;
    let idle = builder.atomic("idle", parent)?;
    let regions = builder.parallel("regions", parent)?;
    let left = builder.compound("left", regions)?;
    let left_idle = builder.atomic("left_idle", left)?;
    let right = builder.compound("right", regions)?;
    let right_idle = builder.atomic("right_idle", right)?;
    let done = builder.final_state("done", root)?;

    builder.initial(root, parent)?;
    builder.initial(parent, idle)?;
    builder.initial(left, left_idle)?;
    builder.initial(right, right_idle)?;
    builder.initial_actions(parent, [StateTreeAction::raise("parent.ready")])?;
    builder.entry_actions(parent, [StateTreeAction::raise("parent.entered")])?;
    builder.exit_actions(parent, [StateTreeAction::raise("parent.exited")])?;
    builder.add_transition(idle, StateTreeTransition::on("activate").target(regions))?;
    builder.add_transition(parent, StateTreeTransition::on("finish").target(done))?;

    builder.build()
}

fn transition_definition() -> Result<StateTreeDefinition, BuildError> {
    let mut builder = StateTreeBuilder::new("transitions");
    builder.luau_datamodel([
        StateTreeData::new("count").with_initializer("0"),
        StateTreeData::new("last"),
    ])?;
    let root = builder.compound_root("#document")?;
    let source = builder.atomic("source", root)?;
    let parent = builder.compound("parent", root)?;
    let child = builder.atomic("child", parent)?;
    let done = builder.final_state("done", root)?;

    builder.initial(root, source)?;
    builder.initial(parent, child)?;
    builder.add_transition(
        source,
        StateTreeTransition::on_any(["advance", "advance.fast"])
            .condition("_event.data.amount > count")
            .target(child)
            .actions([StateTreeAction::assign("count", "count + 1")]),
    )?;
    builder.add_transition(
        source,
        StateTreeTransition::on("observe")
            .actions([StateTreeAction::assign("last", "_event.name")]),
    )?;
    builder.add_transition(source, StateTreeTransition::on("combat"))?;
    builder.add_transition(source, StateTreeTransition::on("navigation.*"))?;
    builder.add_transition(source, StateTreeTransition::on("*"))?;
    builder.add_transition(
        child,
        StateTreeTransition::eventless()
            .condition("count > 0")
            .target(done),
    )?;

    builder.build()
}

fn action_definition() -> Result<StateTreeDefinition, BuildError> {
    let mut builder = StateTreeBuilder::new("actions");
    builder.luau_datamodel([StateTreeData::new("value").with_initializer("1")])?;
    let root = builder.compound_root("#document")?;
    let parent = builder.compound("parent", root)?;
    let source = builder.atomic("source", parent)?;
    let done = builder.final_state("done", root)?;

    builder.initial(root, parent)?;
    builder.initial(parent, source)?;
    builder.entry_actions(
        source,
        [StateTreeAction::effect(
            "record",
            [
                StateTreeEffectArgument::new("first", "value"),
                StateTreeEffectArgument::new("second", "value * 2"),
            ],
        )],
    )?;
    builder.exit_actions(source, [StateTreeAction::raise("source.exited")])?;
    builder.entry_actions(done, [StateTreeAction::effect("finished", [])])?;
    builder.initial_actions(
        parent,
        [
            StateTreeAction::assign("value", "value + 1"),
            StateTreeAction::raise("initialized"),
        ],
    )?;
    builder.add_transition(
        source,
        StateTreeTransition::on("initialized")
            .target(done)
            .actions([
                StateTreeAction::assign("value", "value + 1"),
                StateTreeAction::raise("transition.ran"),
            ]),
    )?;

    builder.build()
}

fn minimal_luau_definition() -> Result<StateTreeDefinition, BuildError> {
    let mut builder = StateTreeBuilder::new("minimal-luau");
    builder.luau_datamodel([])?;
    let root = builder.compound_root("#document")?;
    let state = builder.atomic("#anonymous0", root)?;
    builder.initial(root, state)?;
    builder.add_transition(state, StateTreeTransition::eventless().condition("false"))?;
    builder.build()
}
