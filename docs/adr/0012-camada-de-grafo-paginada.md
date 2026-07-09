# ADR 0012 — Grafo: entidades e relações em páginas próprias, explícitas no `remember`

**Status:** Aceito (jul/2026). Story S13 / item 3.1 do [ROADMAP](../../ROADMAP.md) —
o diferencial de profundidade (vetor + texto + **grafo**) que nenhum embarcado
tem completo.

## Contexto

O M3 adiciona a camada de grafo: memórias podem ser marcadas com **entidades**
("postgres", "auth-service") e ligadas entre si por **relações** tipadas
explícitas. O caller navega `related(id | entity)` e o `recall` pode expandir
1 salto para puxar contexto conectado. Duas questões a decidir: *onde* o grafo
vive e *como* as arestas nascem.

1. **Extração automática de entidades/relações (NER/LLM)?** Não nesta story —
   é lossy, caro, e violaria "nada sai da máquina" se dependesse de API
   externa. Entidades e relações são **explícitas**, fornecidas pelo caller no
   `remember`. Extração local pode vir depois sem mudar o formato.
2. **Grafo em estrutura externa (sidecar, sqlite, petgraph em RAM)?** Mesma
   análise do ADR 0011: qualquer verdade de commit fora do WAL único cria o
   meio-estado irrecuperável que o moat proíbe; qualquer arquivo extra quebra
   "um arquivo"; reconstruir em RAM no open quebra "abrir sem carregar tudo"
   (ADR 0002).

## Decisão

**Grafo próprio, persistido em páginas do `.mind`, integrado ao WAL, com
entidades e relações explícitas gravadas na mesma transação do `remember`.**

### Formato (detalhe normativo em [FORMAT.md §12](../FORMAT.md))

- Dois page types novos: **`GRAPH_DICT` (0x0A)** e **`GRAPH_OVERFLOW` (0x0B)**.
- `graph_root_page` no header (offset 164, reservado-e-zero até a v2) aponta
  para uma **meta page fixa** (contagens + raiz do dicionário) — mesma ideia
  da `HNSW_META` e da meta FTS.
- O dicionário do grafo **reutiliza o mesmo layout de B-tree slotted por bytes
  do dicionário full-text** (FORMAT.md §11), extraído para um módulo
  compartilhado — mesma mecânica de split provável, mesmo transbordo de
  valores grandes para uma cadeia de overflow. Só mudam os page types e o
  conteúdo dos valores. Uma estrutura já fuzzada e testada em produção em vez
  de uma segunda árvore com bugs próprios.
- Duas famílias de chave no mesmo dicionário (1 byte de tag + bytes):
  - `0x01 + nome da entidade` → lista de membros (ids das memórias marcadas);
  - `0x02 + ULID da memória` → adjacência (entidades da memória + arestas
    de relação, ambas as direções).
- Cada relação é gravada **nas duas pontas** (out na origem, in no destino),
  na mesma transação — navegação bidirecional com uma leitura de valor,
  sem varredura.

### Integração transacional

- `remember` grava registro, vetor, full-text **e grafo** numa única transação:
  ou a memória entra com suas entidades/relações completas, ou nada entra.
- Páginas do grafo entram no WAL como quaisquer outras; recovery as reaplica;
  sem journal separado (mesmo princípio do HNSW e do FTS).
- Relação para memória inexistente (ou já esquecida) é **erro tipado** no
  `remember` — arestas penduradas não nascem por construção; elas só surgem
  via `forget` posterior, e aí valem as regras abaixo.

### Deleção e degradação

- Sem delete, igual ao resto da engine: `forget` é tombstone (ADR 0003).
  Relação para memória esquecida **some junto do tombstone**: `related` e a
  expansão do `recall` re-checam a liveness do alvo na hora da query (mesma
  closure `keep` dos outros índices) e nunca devolvem um alvo tombstoned.
- `embedmind vacuum` reconstrói o grafo como reconstrói HNSW e FTS: entidades
  de memórias mortas e arestas com qualquer ponta morta são fisicamente
  descartadas na cópia compactada.
- **`format_version` sobe de 2 para 3.** Um arquivo v2 não tem grafo:
  `graph_root_page` é 0 (bytes eram reservados-e-zero), então um build v3 o lê
  e escreve normalmente e `related`/expansão degradam para vazio até o arquivo
  ser reescrito. Bump **aditivo** (FORMAT.md §10 regra 1), nenhum byte
  pré-existente muda de significado.

## Alternativas rejeitadas

- **Extração automática de entidades:** fora de escopo da S13 (spec S13);
  explícito primeiro valida o formato e a navegação sem acoplar a engine a um
  modelo de NER. Pode ser adicionado depois só no shell/bindings.
- **Guardar entidades/relações dentro do registro (§5):** faria o `related`
  varrer a B-tree inteira (O(N) por navegação) e reescrever registros ao
  criar arestas de entrada — registros são imutáveis pós-`remember` por
  design. Índice separado com as duas direções materializadas é O(log N).
- **Segunda implementação de B-tree para o grafo:** duplicaria ~400 linhas da
  estrutura mais delicada do formato. Extrair o dicionário do FTS para módulo
  compartilhado mantém uma única implementação (mesmos bytes em disco para o
  FTS — refactor puro, corpus de fuzz continua válido).

## Consequências

- O grafo herda as garantias do formato: checksum por página (G1),
  crash-safety pelo WAL (G2), portabilidade (G3), política de versão (G4).
- Novo fuzz target (`fuzz_graph_page`) cobre os parsers novos (meta, corpos de
  entidade/adjacência, cadeia de overflow); o crash harness de registros passa
  a exercitar páginas de grafo porque `remember` as escreve na mesma transação.
- A camada MCP continua casca (CLAUDE.md decisão 2): grafo exposto pela API da
  engine (`Store::related`, `Query::expand_related`).
