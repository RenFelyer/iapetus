use super::*;

#[test]
fn dotenv_takes_plain_quoted_and_exported_values_and_skips_the_rest() {
    let text = "# комментарий\n\nA=1\nexport B = two words \nC=\"q # not a comment\"\nD='single'\nE=\"unpaired\nno separator\n=empty name\nA=last\n";
    let vars = parse_dotenv(text);
    let expected: BTreeMap<String, String> = [
        ("A", "last"),
        ("B", "two words"),
        ("C", "q # not a comment"),
        ("D", "single"),
        ("E", "\"unpaired"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    assert_eq!(vars, expected, "файл .env разобран неверно");
}
