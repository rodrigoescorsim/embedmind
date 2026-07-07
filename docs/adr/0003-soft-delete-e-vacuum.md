# ADR 0003 — `forget` = soft-delete (tombstone) + vacuum offline

**Status:** Aceito (jul/2026)

## Contexto

HNSW não suporta remoção barata: apagar um nó exige religar vizinhanças em múltiplas
camadas sem degradar o grafo. `forget` precisa existir desde a v0.1 (é uma das três
tools MCP), mas remoção online correta é um projeto em si.

## Decisão

`forget` marca o record com bit de tombstone; a busca filtra tombstones (com `ef_search`
adaptativo se a taxa passar de 20%). Espaço e nós do índice são recuperados por
`embedmind vacuum`, que reconstrói páginas e índice HNSW offline (operação por cópia,
crash-safe).

## Alternativas rejeitadas

- **Remoção online no HNSW:** complexidade alta, risco de degradação silenciosa do grafo;
  não paga na v0.x.
- **Hard delete só no B-tree, deixando o nó órfão no índice:** dessincroniza dado e
  índice — viola o invariante I3 do harness de crash.

## Consequências

- Comportamento honesto e documentado: espaço só volta no vacuum.
- Vacuum vira também o caminho de manutenção (rebuild de índice, futura recompactação).
- Custo diferido aceitável: workload de memória de agente deleta pouco.
