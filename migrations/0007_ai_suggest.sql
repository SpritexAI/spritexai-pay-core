-- SpritEXAI Pay — Phase 2: regex auto-suggestion.
-- Author: Mohammad Sijan (SpritexAI). Postgres-portable.
--
-- To propose an updated regex from drifted formats we need the actual SMS text,
-- not just its fingerprint. Only regex-MISSES reach the AI fallback, so this
-- column is bounded to drift samples — the exact data a maintainer needs to fix
-- a parser. ponytail: stored plaintext for parser authoring; prune old rows if
-- the drift log grows, or encrypt-at-rest when Cloud multi-tenant lands.
ALTER TABLE ai_parse_log ADD COLUMN raw_body TEXT;
