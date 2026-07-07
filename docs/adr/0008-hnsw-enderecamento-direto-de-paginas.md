# ADR 0008 — HNSW com endereçamento direto de páginas (sem tabela de localização)

**Status:** Aceito (jul/2026). Complementa o ADR 0002; altera o layout do §7 do
[FORMAT.md](../FORMAT.md) **antes** do v0.1 (spec ainda DRAFT — nenhum arquivo público
existe, `format_version` permanece 1).

## Contexto

O primeiro rascunho do §7 dava a cada nó HNSW um `node_id: u32` lógico e mantinha na
página HNSW_META uma tabela densa `node_id → (page_no, slot)`. Dois problemas
estruturais apareceram na implementação do M1 item 1.3:

1. **Teto de capacidade:** a tabela cabia numa única página — ~405 nós a 4 KiB.
   Encadear via `next_page` (como o rascunho previa) removeria o teto, mas não o
   problema 2.
2. **Custo de escrita O(n):** a tabela inteira era reserializada a cada insert —
   quanto maior o índice, mais caro gravar uma memória. Com encadeamento ingênuo,
   cada insert regravaria a cadeia toda.

Qualquer solução baseada em tabela (encadeada, com diretório de dois níveis, ou
B-tree secundária `node_id → page`) mantém uma estrutura que cresce com o índice,
precisa de I/O própria e é mais uma coisa para corromper, fuzzar e recuperar.

## Decisão

**Eliminar a tabela: a adjacência do grafo referencia páginas diretamente.**

- Vizinhos são `page_no: u64` de páginas HNSW_NODE, não ids lógicos.
- O entry point na meta é `entry_point_page: u64`.
- A página HNSW_META passa a ter **tamanho fixo para sempre**: parâmetros
  (`M`, `ef_construction`), `max_layer`, `entry_point_page`, `node_count`.
- O nível de um nó é limitado por `max_hnsw_level(page_size, M)` para que um nó
  *cheio* (todas as camadas no cap) sempre caiba numa página — `encode` de nó
  construído pela engine nunca falha.

É a mesma ideia que faz grafos ANN residentes em disco funcionarem (DiskANN e
família): o identificador do nó É o seu endereço físico.

Junto com esta mudança, a seleção de vizinhos passou do "manter os M mais próximos"
para a **heurística de diversidade do paper** (Algoritmo 4 + `keepPrunedConnections`,
como hnswlib/faiss) — melhora recall em dados clusterizados (embeddings de texto)
sem nenhum custo de formato.

## Consequências

- **Insert:** toca O(M) páginas (nó novo + vizinhos religados + vetor + meta),
  independentemente do tamanho do índice. Sem teto de nós (u64).
- **Busca:** um hop = uma leitura de página; uma indireção a menos que no desenho
  com tabela.
- **Meta:** nunca cresce, nunca encadeia, cabe em qualquer `page_size` suportado.
- **Vacuum:** mover páginas de nós invalida a adjacência — o vacuum reconstrói o
  índice de qualquer forma (ADR 0003), então nada muda.
- **Custo aceito:** vizinhos ocupam 8 bytes em vez de 4. Irrelevante hoje (um nó por
  página); se o packing de nós (futuro, guiado por benchmark) apertar, u48/u32
  relativo seria um novo ADR.
- `node_id` deixa de existir no formato; a semente do nível determinístico é o
  ordinal de inserção (`node_count` no momento do insert).

## Alternativas rejeitadas

- **Tabela encadeada conforme o rascunho do §7:** remove o teto mas mantém escrita
  O(n) (ou exige protocolo de append parcial na cadeia), mais uma estrutura para
  recuperar/fuzzar, e uma indireção extra por hop.
- **Meta fixa + cadeia append-only de localizações:** resolve o custo de escrita,
  mas mantém a indireção e o código da cadeia; só valeria a pena se ids lógicos
  compactos (u32) fossem necessários — não são, com um nó por página.
- **B-tree secundária `node_id → page`:** escala, mas adiciona um lookup de árvore
  por hop de busca e duplica a maquinaria de árvore para um mapeamento que o
  endereçamento direto dá de graça.
