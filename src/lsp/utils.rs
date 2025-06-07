use lsp_types::MarkedString;

pub fn format_marked_string(marked_string: &MarkedString) -> String {
    match marked_string {
        MarkedString::String(s) => s.clone(),
        MarkedString::LanguageString(language_string) => format!(
            "```{}```\n{}",
            language_string.language, language_string.value
        ),
    }
}
