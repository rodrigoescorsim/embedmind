# ADR 0005 — Reciprocal Rank Fusion (RRF) para o recall híbrido

**Status:** Aceito (jul/2026) — aplica-se ao M2 (full-text + metadados)

## Contexto

A partir do M2 o `recall` combina duas listas ranqueadas: similaridade vetorial (HNSW)
e full-text (BM25). Combinar scores de naturezas diferentes (coseno vs. BM25) exige
normalização ou pesos — e pesos calibrados viram dívida de manutenção e comportamento
inexplicável para o usuário.

## Decisão

Fusão por **Reciprocal Rank Fusion** com `k=60`: cada documento soma
`1/(k + rank)` em cada lista onde aparece. Só usa posições de ranking, nunca os scores
brutos — nada a normalizar, nada a calibrar.

## Alternativas rejeitadas

- **Combinação linear de scores normalizados:** exige calibrar pesos por workload; quebra silenciosamente quando a distribuição de scores muda (outro modelo de embedding, outro corpus).
- **Pesos aprendidos:** precisa de dados de relevância que não existem num produto local-first sem telemetria.

## Consequências

- Zero tuning, comportamento explicável ("apareceu bem nas duas listas"), robusto a troca de modelo.
- RRF é teto conhecido, não ótimo — se o dogfooding mostrar casos ruins, um ADR futuro pode introduzir boost explícito por filtro de metadados (que é determinístico, não peso mágico).
