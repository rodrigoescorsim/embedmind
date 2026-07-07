# ADR 0002 — HNSW próprio, persistido em páginas

**Status:** Aceito (jul/2026)

## Contexto

A busca vetorial precisa de um índice ANN. Existem crates prontos (`hnsw_rs`, `usearch`),
mas todos assumem o grafo inteiro em RAM com serialização monolítica (carregar tudo →
usar → salvar tudo). A promessa do EmbedMind é abrir um arquivo de 1 GB instantaneamente,
sem carregar tudo, com inserções duráveis via WAL.

## Decisão

Implementar HNSW próprio, com nós persistidos em páginas do próprio arquivo
([FORMAT.md](../FORMAT.md) §7). Mutações do grafo são escritas de página comuns e entram
no WAL como quaisquer outras. Parâmetros default: `M=16`, `ef_construction=200`,
`ef_search=64`, distância coseno (vetores normalizados na inserção).

## Alternativas rejeitadas

- **Biblioteca externa in-memory:** quebra cold-open rápido, quebra durabilidade
  transacional do índice (índice e dados dessincronizam após crash), e terceiriza
  exatamente a barreira técnica que é o moat do projeto.
- **IVF/flat com rerank:** mais simples, mas recall/latência piores no perfil de uso
  (dezenas a centenas de milhares de memórias curtas, consultas one-shot).

## Consequências

- ~800 linhas críticas sob nosso controle e cobertas por property tests (recall@10 ≥ 0.9 vs. busca exata).
- Remoção online é cara em HNSW → motivou o ADR 0003 (tombstone + vacuum).
- Custo: implementar e manter; aceito porque o índice paginado É o diferencial técnico.
