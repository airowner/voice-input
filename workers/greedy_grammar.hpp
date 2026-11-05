#pragma once

#include "protocol.hpp"
#include "llama.h"
#include "output_grammar.hpp"
#include <cmath>
#include <memory>

namespace voice {
// Equivalent to grammar -> greedy. Grammar.apply never accepts a token: every
// selected token, including EOG, reaches grammar.accept exactly once below.
class GreedyGrammar {
    using Sampler = std::unique_ptr<llama_sampler, decltype(&llama_sampler_free)>;
    Sampler grammar;
    Sampler greedy;
    const int32_t vocabulary_size;
    std::vector<llama_token_data> buffer;

public:
    explicit GreedyGrammar(const llama_vocab * vocab)
        : grammar(llama_sampler_init_grammar(vocab, voice_output_grammar, "root"), llama_sampler_free),
          greedy(llama_sampler_init_greedy(), llama_sampler_free),
          vocabulary_size(llama_vocab_n_tokens(vocab)) {
        if (!grammar || !greedy || vocabulary_size <= 0) throw Failure{"output_grammar_failed", true};
    }

    llama_token sample(llama_context * context) {
        const float * logits = llama_get_logits_ith(context, -1);
        if (!logits) throw Failure{"missing_logits", true};
        llama_token best = 0;
        for (llama_token token = 1; token < vocabulary_size; ++token) {
            if (logits[token] > logits[best]) best = token;
        }
        llama_token_data candidate{best, logits[best], 0.0F};
        llama_token_data_array single{&candidate, 1, -1, false};
        llama_sampler_apply(grammar.get(), &single);
        if (single.size == 1 && single.data[0].id == best
                && !std::isnan(single.data[0].logit)
                && single.data[0].logit != -INFINITY) {
            llama_sampler_accept(grammar.get(), best);
            return best;
        }
        // The highest raw logit was invalid. Retain the complete old algorithm;
        // do not approximate with top-k or accept an unconstrained fallback.
        buffer.resize(static_cast<size_t>(vocabulary_size));
        for (llama_token token = 0; token < vocabulary_size; ++token) {
            buffer[token] = llama_token_data{token, logits[token], 0.0F};
        }
        llama_token_data_array all{buffer.data(), buffer.size(), -1, false};
        llama_sampler_apply(grammar.get(), &all);
        llama_sampler_apply(greedy.get(), &all);
        if (all.selected < 0 || static_cast<size_t>(all.selected) >= all.size
                || std::isnan(all.data[all.selected].logit)
                || all.data[all.selected].logit == -INFINITY) {
            throw Failure{"output_grammar_failed", true};
        }
        const llama_token selected = all.data[all.selected].id;
        llama_sampler_accept(grammar.get(), selected);
        return selected;
    }
};
} // namespace voice
