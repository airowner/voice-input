#pragma once

// Output shape only. Rust still validates size, controls, completeness, and facts.
inline constexpr const char * voice_output_grammar = R"gbnf(
root ::= ws "{" ws "\"text\"" ws ":" ws string ws "}" ws
string ::= "\"" char* "\""
char ::= [^"\\\x00-\x1F\x7F] | "\\" (["\\/bfnrt] | "u" [0-9a-fA-F]{4})
ws ::= [ \t\n\r]{0,16}
)gbnf";
