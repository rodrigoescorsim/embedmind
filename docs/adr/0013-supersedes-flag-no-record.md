# ADR 0013 — `supersedes`: flag no record do alvo, exclusão re-verificada no registro

**Status:** Aceito (jul/2026). Story S19 / fase FR1 ([01-spec.md](../01-spec.md),
[03-tasks.md](../03-tasks.md)) — "conhecimento versionado" como diferencial de
launch: corrigir um fato sem perder o histórico.

## Contexto

`remember(supersedes: [id])` grava a memória nova e precisa (1) excluir o alvo
de todo `recall` subsequente, (2) preservá-lo como histórico legível
(`get`/`related`), (3) deixar a cadeia de versões navegável nos dois sentidos,
e (4) sobreviver a `forget` e `vacuum` com semântica previsível. A questão de
design: **onde vive o estado "superseded"?**

Três representações candidatas:

1. **Flag no record do alvo** (bit 1 de `flags`, ao lado do tombstone).
2. **Índice de exclusão separado** (página/dicionário próprio com os ids
   excluídos).
3. **Derivar do grafo** (excluído = "existe aresta `supersedes` entrando,
   vinda de uma memória viva").

## Decisão

**Flag no record (bit 1 de `flags`, `FORMAT.md` §5) + aresta `"supersedes"`
na camada de grafo (S13/ADR 0012), gravados na mesma transação do `remember`.**

- A **flag é a autoridade** da exclusão: `recall`, `search_text`,
  `recall_vector` e a expansão 1-hop re-checam `superseded` no próprio record
  no momento da busca — exatamente a regra dos tombstones (S2). Nenhum índice
  (HNSW, FTS, grafo) é fonte de verdade de exclusão.
- A **aresta é só navegação**: `related(B)` mostra `supersedes → A` e
  `related(A)` o inverso. Uma relação com kind `"supersedes"` criada
  manualmente via `relations` NÃO exclui ninguém — sem a flag, é uma aresta
  comum.
- Validação dentro da transação, antes de qualquer escrita: alvo inexistente
  ou tombstoned → erro tipado (mesma regra das relations); alvo de **outro
  projeto** (inclusive global vs. escopado) → erro tipado; falha em qualquer
  alvo da lista faz rollback completo. Alvos duplicados são deduplicados.
- A escrita da flag reusa o mesmo caminho de rewrite de record que o `forget`
  já usa para o tombstone — `superseded` é o segundo bit de estado do record,
  não um mecanismo novo.

### Interação com `forget` e `vacuum` (bordas da spec S19)

- **`forget` do substituto NÃO ressuscita o substituído.** A exclusão de A é
  estado próprio no record de A, não uma travessia a partir de B. Default
  deliberado: "B estava errado" se corrige com um novo `remember` (que pode
  supersedes B, ou re-afirmar A), nunca com um efeito colateral silencioso de
  `forget` — ressuscitar automaticamente reintroduziria conhecimento antigo
  sem nenhum julgamento humano/agente no laço.
- **`vacuum` PRESERVA superseded** (histórico, não lixo): a cópia compactada
  leva o record com a flag intacta e a aresta `supersedes` (as duas pontas
  estão vivas). `forget` explícito de uma memória superseded continua
  possível e aí sim o vacuum a reclama.

### Formato

Sem página nova, sem campo novo, sem bump de `format_version`: o bit 1 de
`flags` era reservado-escrito-zero (§5), então a mudança cai na regra 1 da
política de evolução do FORMAT.md ("dá para expressar com bytes/flags
reservados → sem bump"). Arquivo antigo decodifica `superseded = false` em
tudo (correto). Leitor antigo de um arquivo novo ignora o bit — memórias
superseded voltariam ao recall naquele build antigo; degradação documentada,
nunca corrupção. Nenhum parser novo → nenhum fuzz target novo; o `fuzz_record`
existente cobre o byte de flags e o corpus ganhou uma seed com o bit ligado.

## Alternativas rejeitadas

- **Índice de exclusão separado (2):** duplica a fonte de verdade — o mesmo
  fato ("este id não aparece em busca") passaria a existir em dois lugares
  (record + índice) com janela de divergência a cada crash, exigindo página
  nova, parser novo, fuzz target novo e reconciliação no recovery. A flag no
  record dá a mesma re-verificação O(1) que o tombstone já paga (o record já
  é lido pela closure `keep` de toda busca) sem nenhuma estrutura nova.
- **Exclusão derivada do grafo (3):** violaria a regra "exclusão re-verificada
  contra o registro" (S2/S19): a resposta dependeria do índice de grafo e de
  uma travessia por busca; e `forget` do substituto ressuscitaria o
  substituído implicitamente (a aresta some com o tombstone da origem) — a
  alternativa exata que a spec S19 rejeita.
- **Reusar o tombstone (marcar A como forgotten):** perderia o histórico —
  `get(A)`/`related(A)` morreriam e o `vacuum` apagaria A fisicamente,
  contrariando o requisito de "versão anterior navegável".

## Consequências

- `Memory`/`MemoryRecord` ganham `superseded: bool`; `in_scope` (a metade
  liveness/escopo de toda `keep`) exclui superseded junto com tombstone —
  qualquer busca futura que use `keep` herda a regra de graça (a lista de
  recência da S20 inclusive, ver borda da S20).
- Cascas: MCP `remember` aceita `supersedes: [ids]` (aditivo, retrocompatível);
  CLI `remember --supersedes ID` repetível; `related` do CLI marca vizinhos
  `[superseded]`.
- Crash harness novo (`crash_supersede.rs`): o rewrite do record do alvo, as
  páginas de grafo da aresta e o insert do record novo viajam na mesma
  transação injetada — atomicidade "flag + memória nova, ou nada" verificada
  por snapshot exato.
- Cadeias (A←B←C) funcionam por indução: cada supersede flagou seu alvo no
  momento da escrita; só a cabeça (não-flagada) aparece em busca; a cadeia se
  percorre passo a passo via `related`.
