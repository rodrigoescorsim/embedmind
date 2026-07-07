# ADRs — Architecture Decision Records

Registro das decisões de arquitetura do EmbedMind. Um arquivo por decisão, imutável após
aceito (mudou de ideia → novo ADR que supersede o anterior). Doc interno, em português,
conforme convenção do [CLAUDE.md](../../CLAUDE.md).

O [DESIGN.md](../DESIGN.md) §11 mantém a tabela-resumo; a versão completa de cada decisão
vive aqui. As questões em aberto do DESIGN §12 viram novos ADRs quando resolvidas
(próximos números: 0008+).

| # | Decisão | Status |
|---|---|---|
| [0001](0001-wal-fisico-de-paginas.md) | WAL físico de páginas, não log lógico | Aceito |
| [0002](0002-hnsw-proprio-paginado.md) | HNSW próprio persistido em páginas | Aceito |
| [0003](0003-soft-delete-e-vacuum.md) | `forget` = soft-delete + vacuum offline | Aceito |
| [0004](0004-modelo-de-embedding-embarcado.md) | Modelo de embedding embarcado (MiniLM int8) | Aceito |
| [0005](0005-rrf-para-fusao-hibrida.md) | RRF para fusão híbrida de scores | Aceito |
| [0006](0006-single-writer.md) | Single-writer / multi-reader, sem MVCC | Aceito |
| [0007](0007-criptografia-reservada-no-formato.md) | Criptografia reservada no formato, não implementada | Aceito |

Template: `Status · Contexto · Decisão · Alternativas rejeitadas · Consequências`.
