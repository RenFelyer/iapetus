use super::*;

#[test]
fn id_parses_back_to_the_same_agent_and_other_text_is_refused() {
    let ids = Agent::ALL.map(Agent::id);
    assert_eq!(ids, ["claude", "codex", "antigravity"], "id разошёлся с реестром и метками Docker");
    for agent in Agent::ALL {
        assert_eq!(agent.id().parse::<Agent>(), Ok(agent), "id {:?} не разобран обратно", agent.id());
    }
    for bad in ["", "Claude", "claude-code", "gemini", " codex"] {
        assert!(bad.parse::<Agent>().is_err(), "строка {bad:?} принята за агента");
    }
}
