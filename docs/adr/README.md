# ADRs — Architecture Decision Records

Registro das decisões de arquitetura do EmbedMind. Um arquivo por decisão, imutável após
aceito (mudou de ideia → novo ADR que supersede o anterior). Doc interno, em português,
conforme convenção do [CLAUDE.md](../../CLAUDE.md).

O [DESIGN.md](../DESIGN.md) §11 mantém a tabela-resumo; a versão completa de cada decisão
vive aqui. As questões em aberto do DESIGN §12 viram novos ADRs quando resolvidas
(próximos números: 0014+).

| # | Decisão | Status |
|---|---|---|
| [0001](0001-wal-fisico-de-paginas.md) | WAL físico de páginas, não log lógico | Aceito |
| [0002](0002-hnsw-proprio-paginado.md) | HNSW próprio persistido em páginas | Aceito |
| [0003](0003-soft-delete-e-vacuum.md) | `forget` = soft-delete + vacuum offline | Aceito |
| [0004](0004-modelo-de-embedding-embarcado.md) | Modelo de embedding embarcado (MiniLM int8) | Aceito |
| [0005](0005-rrf-para-fusao-hibrida.md) | RRF para fusão híbrida de scores | Aceito |
| [0006](0006-single-writer.md) | Single-writer / multi-reader, sem MVCC | Aceito |
| [0007](0007-criptografia-reservada-no-formato.md) | Criptografia reservada no formato, não implementada | Aceito |
| [0008](0008-hnsw-enderecamento-direto-de-paginas.md) | HNSW com endereçamento direto de páginas (sem tabela de localização) | Aceito |
| [0009](0009-mcp-stdio-direto-sem-sdk.md) | Servidor MCP: stdio JSON-RPC direto, sem SDK (sem tokio) | Aceito |
| [0010](0010-teto-de-tamanho-governa-artefato-comprimido.md) | Teto de tamanho (< 40 MB) governa o artefato comprimido de release | Aceito |
| [0011](0011-full-text-indice-invertido-proprio.md) | Full-text: índice invertido próprio nas páginas (BM25), não tantivy | Aceito |
| [0012](0012-camada-de-grafo-paginada.md) | Grafo: entidades e relações em páginas próprias, explícitas no `remember` | Aceito |
| [0013](0013-supersedes-flag-no-record.md) | `supersedes`: flag no record do alvo, exclusão re-verificada no registro | Aceito |
| [0014](0014-recencia-terceira-lista-rrf.md) | Recência como terceira lista na fusão RRF do recall | Aceito |
| [0015](0015-ef-search-default-escalado-pelo-indice.md) | `ef_search` default escalado pelo tamanho do índice (patamares medidos) | Aceito |
| [0016](0016-limiar-de-near-duplicate-medido.md) | Limiar de near-duplicate do `remember` medido no corpus (0.80) | Aceito |
| [0017](0017-otimizacao-do-full-text-escopo-e-metodo.md) | Otimização do full-text: profiling antes de estrutura, bump de formato liberado | Aceito |
| [0018](0018-early-termination-no-scan-bm25.md) | Early termination no scan BM25: avaliação limitada por bound, resultado idêntico | Aceito |
| [0019](0019-recall-tie-aware-no-harness.md) | Recall@k do harness tie-aware: paridade de score contra o top-k exato | Aceito |
| [0020](0020-rss-de-pico-era-o-harness-nao-o-engine.md) | RSS de pico @ 100k era o harness (VectorSet de baseline), não o engine | Aceito |
| [0021](0021-postings-fts-delta-varint.md) | Postings FTS comprimidas com delta+varint (`format_version` 4) | Aceito |
| [0022](0022-postings-fts-skip-lists.md) | Skip lists nas postings FTS grandes (`format_version` 5) | Aceito |
| [0023](0023-blockmax-wand-decisao-fase-bmw.md) | BlockMax-WAND para fechar o NFR de latência do full-text (decisão do founder) | Aceito |
| [0024](0024-bound-de-impacto-por-bloco-fv6.md) | Bound de impacto por bloco no skip index FTS (`format_version` 6) | Aceito |
| [0025](0025-blockmax-wand-na-busca-fts.md) | BlockMax-WAND na busca full-text (BMW-2) | Aceito |

Template: `Status · Contexto · Decisão · Alternativas rejeitadas · Consequências`.
