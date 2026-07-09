# ADR 0011 — Full-text: índice invertido próprio nas páginas, não tantivy

**Status:** Aceito (jul/2026). Resolve a questão aberta do DESIGN §12
("Full-text próprio vs. tantivy") no default previsto. Metade-engine da story
S9 / item 2.3 do [ROADMAP](../../ROADMAP.md).

> Nota de numeração: o rascunho da task pedia "ADR 0010", mas esse número já
> tinha sido tomado pela decisão do teto de tamanho de release. Este é o
> próximo número livre, conforme o [README dos ADRs](README.md).

## Contexto

O item 2.3 do M2 adiciona busca full-text (keyword) à engine, para depois ser
fundida com a busca vetorial via RRF (ADR 0005). Precisamos de um índice
invertido `termo → documentos` com scoring, e de uma decisão sobre *onde* esse
índice vive. Duas rotas:

1. **Embutir `tantivy`** (a "Lucene do Rust"): madura, rápida, BM25 pronto,
   analisadores prontos. Mas é um **motor de busca com armazenamento próprio**:
   escreve seus próprios segmentos imutáveis em vários arquivos num diretório,
   com o próprio ciclo de commit/merge. Isso colide de frente com dois pilares
   do produto:
   - **"Um arquivo" (`.mind`)** — DESIGN §1, promessa de produto. Um diretório
     de segmentos tantivy ao lado do `.mind` quebra a promessa; embutir os
     segmentos *dentro* do `.mind` significaria reimplementar o storage do
     tantivy, anulando o motivo de usá-lo.
   - **Crash-safety via WAL único** — DESIGN decisão #1, CLAUDE.md decisão #5.
     O tantivy tem durabilidade própria, fora do nosso WAL. Teríamos **duas
     verdades de commit** desacopladas: um crash entre o commit do `.mind` e o
     commit do tantivy deixa índice e registros divergentes — exatamente o
     "meio-estado irrecuperável" que o moat de confiabilidade proíbe.
   - Custo extra: puxa uma árvore de dependências grande (viola o orçamento
     curto do DESIGN §10) e infla o binário (o teto de 40 MB do ADR 0010).

2. **Índice invertido próprio nas páginas do `.mind`** — mesma aposta que já
   fizemos com o HNSW (ADR 0002/0008): a estrutura de índice é páginas do nosso
   formato paginado, e toda mutação passa pelo `Txn`/WAL como qualquer outra
   escrita.

## Decisão

**Implementar um índice invertido próprio, persistido nas páginas do `.mind`,
com scoring BM25, integrado ao WAL.** É o default que o DESIGN §7/§12 já
previa e é consistente com "tudo num arquivo".

### Formato (detalhe normativo em [FORMAT.md §11](../FORMAT.md))

- Dois page types novos: **`FTS_DICT` (0x08)** e **`FTS_POSTINGS` (0x09)**.
- `fts_root_page` no header (offset 156, que era reservado-e-zero na v1) aponta
  para uma **meta page fixa** com as estatísticas de corpus do BM25
  (`doc_count`, `total_tokens`) e a raiz do dicionário — mesma ideia da
  `HNSW_META`: tamanho fixo para sempre.
- O **dicionário** é uma B-tree slotted por bytes de termo (variáveis, ordem
  lexicográfica), espelhando a mecânica de split provável da B-tree de
  registros (FORMAT.md §5.1). As postings de cada termo (`doc_freq` +
  `(record_id, term_freq)` ordenados por id) ficam inline no valor do
  dicionário; quando grandes demais, transbordam para uma cadeia `FTS_POSTINGS`
  — exatamente como um registro grande transborda para `OVERFLOW`.
- Meta/inner/leaf do dicionário compartilham o mesmo page type `FTS_DICT`,
  distinguidos por um byte de tipo de nó no corpo — o índice acrescenta só dois
  page types.

### Integração transacional

- `remember` chama `fts::index_document` **dentro da mesma transação** que grava
  o registro e o vetor. Ou tudo entra, ou nada: sem meio-estado a recuperar.
- As páginas FTS tocadas entram no WAL como quaisquer outras; recovery as
  reaplica; **sem journal de índice separado** — o mesmo princípio do HNSW.

### Scoring BM25

- BM25 padrão (`k1 = 1.2`, `b = 0.75`). `N` e `avgdl` vêm da meta page.
- O comprimento `|D|` de cada documento **não é persistido**: é recomputado
  tokenizando o conteúdo do candidato no momento da query. O recall já lê cada
  candidato para re-checar tombstone/escopo (como o caminho vetorial faz), então
  a contagem de tokens sai de graça ali e há **um dado a menos que pode divergir
  em disco**.

### Deleção e degradação

- Sem delete, igual ao resto da engine: `forget` é tombstone (ADR 0003), e
  postings de registros tombstoned/fora de escopo são filtrados no momento da
  query (closure `keep`), depois reclamados fisicamente pelo `embedmind vacuum`
  (que reconstrói este índice como reconstrói o HNSW).
- **`format_version` sobe de 1 para 2.** Um arquivo v1 não tem índice full-text:
  `fts_root_page` é 0 (bytes eram reservados-e-zero), então um build v2 o lê e
  escreve normalmente e o `recall` degrada para vetorial-apenas até o arquivo
  ser reescrito. Bump **aditivo** (FORMAT.md §10 regra 1), não quebra: nenhum
  byte pré-existente muda de significado.

## Alternativas rejeitadas

- **`tantivy` embutido:** rápido de entregar, mas quebra "um arquivo" e o WAL
  único (duas verdades de commit → meio-estado após crash), infla dependências
  e binário. O ganho de features (analisadores, faceting) não é necessário na
  v0.x e não paga o custo no ativo mais precioso: a confiabilidade.
- **Índice full-text só em memória, reconstruído no open:** rápido, sem formato
  novo — mas reconstruir a cada abertura de um arquivo de 1 GB viola a promessa
  de "abrir sem carregar tudo" (mesmo argumento do ADR 0002 contra HNSW
  in-memory), e o custo de startup cresce sem teto.
- **Stemming/stopwords no tokenizer agora:** lossy e específico de idioma
  (o founder é pt-BR); o IDF do BM25 já reduz o peso de palavras comuns. Fica
  para quando o dogfooding pedir, sem quebra de formato (é só o tokenizer).

## Consequências

- O índice full-text herda de graça as garantias de formato: checksum por
  página (G1), crash-safety pelo WAL (G2), portabilidade byte-idêntica (G3),
  política de versão (G4). Um novo fuzz target (`fuzz_fts_page`) cobre os
  parsers novos; o crash harness de registros passa a exercitar as páginas FTS
  porque `remember` agora as escreve na mesma transação.
- Mais código próprio para manter (a B-tree do dicionário), mas é o mesmo tipo
  de código que já dominamos (slotted pages, split provável, overflow chains) e
  é testável isoladamente.
- Fica sem os analisadores sofisticados do tantivy — aceito: BM25 sobre tokens
  alfanuméricos Unicode-aware resolve o caso de memória de agente, e melhorar o
  tokenizer não quebra o formato.
- A camada MCP continua sem lógica de domínio (CLAUDE.md decisão 2): o full-text
  é exposto pela API da engine (`Store::search_text`), a fundir com o vetorial
  via RRF na outra metade da S9.
