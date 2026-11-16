# Malha P2P: espectadores trocando pedaços de vídeo entre si

Hoje o host manda uma cópia do vídeo para cada espectador: com 8 Mb/s e 5 pessoas, são
40 Mb/s de upload. Na malha, cada quadro vira pedaços codificados que circulam entre os
espectadores: cada um recebe de várias pessoas e repassa para várias, e o host manda cada
pedaço uma vez só. O objetivo é que o upload do host fique perto de ~1,4 stream, seja qual
for o número de espectadores, sem perder a latência baixa (hoje ~15 ms de buffer).

## Ideias centrais

### 1. Pedaços codificados (RaptorQ, RFC 6330)

Cada quadro H.264 é dividido em `k` símbolos de até 1.100 bytes (um pedaço = um pacote
UDP, sem fragmentação no SCTP). O RaptorQ gera, a partir deles, quantos pedaços
codificados se quiser; **quaisquer `k` (às vezes `k`+1 ou 2) pedaços, de qualquer origem e
em qualquer ordem, reconstroem o quadro**. Consequências:

- "Usar o que chegar primeiro": os primeiros `k` pedaços que chegam, de quem for, fecham o
  quadro. Um vizinho lento não atrasa; um vizinho que caiu não trava.
- Folga ajustável: mandar `k + r` pedaços. `r` = ~40% no normal, mais nos keyframes e para
  quem anda travando, menos para quem está estável.
- Reparo sem saber o que se perdeu: o código não tem fim, então quem está a 2 pedaços de
  fechar um quadro pede "mais 2" e qualquer um com o quadro gera pedaços novos.

### 2. Faixas com rotas fixas (inspirado no SplitStream)

Os pedaços de cada quadro são repartidos em `F` faixas (pedaço `i` vai na faixa `i mod F`).
Cada faixa tem a própria rota pela malha, e as rotas são diferentes: quem repassa a faixa 1
é folha nas outras. Resultado: cada espectador recebe de `F` vizinhos diferentes e repassa
para 2 ou 3, sem ninguém perguntar "quem tem o quê" (o que somaria atraso) e sem pedaço
duplicado.

```
Faixa 0: Host → Ana  → Caio, Duda
Faixa 1: Host → Caio → Ana, Edu
Faixa 2: Host → Duda → Bia, Caio
Faixa 3: Host → Edu  → Ana, Bia
```

Com a folga do RaptorQ, perder uma faixa inteira (alguém saiu) não trava o vídeo enquanto
o host refaz aquela rota.

### 3. O host coordena

O host já conhece todo mundo e tem canal de dados com cada um. Ele monta as rotas (quem
repassa qual faixa para quem), a partir dos relatórios que os espectadores já mandam
(banda, perda, atraso) e da capacidade de upload informada. Quem tem pouco upload só
recebe. As conexões espectador↔espectador usam o próprio canal do host para a troca de
oferta e resposta do WebRTC (sem a DHT).

### 4. Detectar e reagir

Cada pedaço traz quadro e faixa, e cada faixa chega de um vizinho conhecido. Então quem
recebe sabe **qual** vizinho está ruim:

- Uma faixa ruim e as outras boas: o problema é aquele vizinho → pede para trocar.
- Todas ruins ao mesmo tempo: o problema é a própria conexão → não troca vizinho; baixa a
  qualidade (as versões 720p/480p que já existem).

Reação em três velocidades: (1) na hora, pede pedaços extras de um quadro que não fechou;
(2) em menos de 1 s, o host troca o vizinho daquela faixa (ou manda ela direto enquanto
isso); (3) com o tempo, nota por pessoa: quem recebe muitas reclamações deixa de repassar.
Com histerese (trocar só depois de o problema durar, esperar depois de uma troca), como a
qualidade automática faz hoje.

### 5. Assinatura

Pedaços passam por outros espectadores, então o host assina cada quadro (Ed25519, já no
projeto). Quem recebe reconstrói o quadro e só entrega ao decoder se a assinatura bater;
quadro adulterado é descartado e o vizinho que mandou pedaços dele perde nota.

## Formato do pedaço

Cabeçalho (binário, little-endian) + símbolo RaptorQ:

| campo | bytes | |
|---|---|---|
| versão | 1 | 1 |
| tipo | 1 | 0 = vídeo, 1 = áudio |
| versão do vídeo | 2 | 0 = principal, senão a altura (720, 480) |
| quadro | 4 | número sequencial do quadro (por versão) |
| timestamp RTP | 4 | 90 kHz, como hoje |
| tamanho do quadro | 4 | bytes |
| símbolo | 2 | tamanho do símbolo (todos iguais no quadro) |
| marcadores | 1 | bit 0 = keyframe |
| faixa | 1 | |
| id do pedaço | 4 | ESI do RaptorQ (`k` primeiros = originais) |

A assinatura do quadro (64 bytes) viaja num pedaço próprio de cada quadro (etapa 2).

## Etapas

Cada etapa termina com testes passando e algo que dá para medir.

### Etapa 0: medir o canal de dados ✅

Teste `transport::tests::datachannel_pieces_throughput` (mesma máquina, canal fora de
ordem e sem reenvio, pedaços de 1.100 bytes a 60 fps):

| alvo | recebido | perda | atraso p50 | p99 |
|---|---|---|---|---|
| 8 Mb/s | 8,4 | 0% | 0,34 ms | 0,60 ms |
| 20 Mb/s | 20,1 | 0% | 0,54 ms | 0,86 ms |
| 40 Mb/s | 40,1 | 0% | 0,76 ms | 1,28 ms |

O canal aguenta o vídeo. Falta ver o comportamento numa rede real (o SCTP tem controle de
congestionamento próprio); isso entra na etapa 1.

### Etapa 1: pedaços codificados do host para o espectador (sem malha) — em andamento

Feito:

- `src/pieces.rs`: codificador e montador (blocos de até 64 pedaços: o plano do RaptorQ
  para k=273 custava 66 ms na primeira vez; com blocos, um keyframe de 300 KB codifica em
  ~1,5 ms) e `Ordered` (entrega na ordem, espera 60 ms por atrasados, pula para o próximo
  keyframe quando perde um quadro).
- Transporte: canal "aj-p"; o host manda pedaços quando `USE_PIECES` está ligado
  (`AJ_PIECES=1`) e o espectador tem o canal; senão RTP. Cada mensagem leva um número
  sequencial do canal (perda medida por ele, no relatório "rr" de sempre).
- Teste `loopback_pieces` (vídeo real 720p60, 240 quadros): 240/240 decodificados sem
  perda e com 5% dos pedaços perdidos (perda medida 5,2%; nenhum keyframe extra pedido).
- App de verdade (autoteste): 1080p60 pelos pedaços, buffer 12 ms, framegen a 120 fps.

Falta: opção na interface, teste numa rede real (entre dois PCs) e o áudio (segue RTP).

Plano original da etapa:

- `src/pieces.rs`: quadro → pedaços (codificador) e pedaços → quadro (montador), com
  folga, limite de memória e descarte de quadros velhos. Testes sem rede: perder qualquer
  `r` pedaços ainda fecha; ordem embaralhada; pedaços de quadros misturados; quadro velho
  descartado; keyframe com mais folga.
- Transporte: canal de dados "aj-p" (fora de ordem, sem reenvio) além do "aj". O host
  manda os pedaços pelo pacer atual; o espectador monta e entrega ao mesmo `on_video` de
  hoje (o resto do caminho não muda: playout, framegen, player).
- Opção na configuração (desligada por padrão) para escolher RTP ou pedaços, e comparar.
- Medidas: perda de pedaços, quadros fechados no prazo, folga usada; o relatório "rr" do
  bitrate adaptativo passa a usar a perda de pedaços.
- Teste ponta a ponta na mesma máquina e com perda simulada.

### Etapa 2: a malha

- Conexões espectador↔espectador (oferta/resposta pelo canal do host), só canal de dados.
- Host: tabela de rotas por faixa; manda a cada espectador "receba a faixa X de Y,
  repasse para Z, W".
- Espectador: repassa os pedaços de cada faixa para os filhos daquela faixa assim que
  chegam (sem esperar o quadro fechar).
- Assinatura dos quadros.
- Quem não consegue conexão direta com outros espectadores recebe do host como hoje.
- Teste com várias instâncias na mesma máquina (modo de autoteste com N espectadores):
  upload do host vs. número de espectadores, atraso por camada.

### Etapa 3: detectar e reagir

- Medidas por vizinho e por faixa; "vizinho ruim" vs. "minha internet ruim".
- Pedido de pedaços extras para quadro que não fechou.
- Troca de vizinho pelo host, nota por pessoa, histerese.
- Autoteste com um "espectador ruim" (descarta ou segura parte dos pedaços): tempo até
  perceber e trocar, quadros perdidos na troca, se confunde as duas situações.

### Etapa 4: folga adaptativa

- Folga por espectador conforme o histórico; mais folga em keyframes; ajuste automático
  pela perda medida (como o controle de bitrate).

## Fora do escopo por enquanto

- IP exposto entre espectadores (decidido: ignorar por enquanto).
- Servidor de repasse (TURN) para quem não consegue conexão direta.
