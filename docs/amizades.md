# Amizades sem servidor

Cada pessoa tem uma conta que é só um par de chaves, guardado no próprio PC. Não existe
servidor de contas: amigos se acham pela DHT do BitTorrent (a mesma do encontro das
transmissões) e, quando os dois estão abertos, conversam direto pelo socket da fofoca
(`docs/gossip-ao-vivo.md`).

## A conta

- **Frase de recuperação**: 24 palavras (BIP39, lista em português). Dela saem a chave
  mestra e a chave de caixa. Quem tem a frase tem a conta.
- **Chave mestra** (Ed25519): a identidade. A parte pública é o código de amigo
  (`AJF.…`). Só fica guardada nos aparelhos *principais* (onde a conta foi criada ou
  restaurada pela frase). Assina os certificados dos aparelhos e o cartão da conta.
- **Chave de caixa** (X25519): cifra o que vai para os amigos. Com a de um amigo, dá o
  segredo do par (Diffie-Hellman). Todos os aparelhos da conta têm.
- **Chave do aparelho** (Ed25519): sorteada por cada aparelho, com um certificado da mestra
  ("este aparelho fala pela conta"). Tudo que um aparelho manda aos amigos vai assinado por
  ela, com o certificado junto. Um aparelho desvinculado entra na lista do cartão e os
  amigos passam a recusar o que ele assina.
- **Impressão**: 4 figuras coloridas e 6 letras (`K7Q·F2M`) tiradas da chave mestra. Nomes
  podem repetir; a impressão não. Serve para conferir de olho que é a pessoa certa.

Guardado em `%APPDATA%\AntiJanja\identity.json` (a entropia da frase, nos principais) e
`friends.json` (amigos e pedidos).

## Na DHT

Itens mutáveis (BEP44, até 1000 bytes), todos cifrados:

| Item | Chave que assina | Salt | Quem lê |
|---|---|---|---|
| Cartão da conta: nome, chave de caixa, aparelhos desvinculados | mestra | `card` | quem tem o código de amigo |
| Caixa de entrada: pedidos de amizade (16 vagas) | derivada do código de amigo | `rq0`…`rq15` | só o dono (cifrado para a chave de caixa dele) |
| Canal do par: presença de A para B | derivada do segredo do par e da conta de A | `p` | só B |

- O cartão é publicado pelo principal a cada 30 min. Os aparelhos vinculados não têm a
  mestra: republicam o mesmo item assinado (a DHT esquece em ~2 h).
- **Pedido**: A lê o cartão de B (precisa da chave de caixa dele), cifra o pedido para ela
  e escreve na vaga da caixa de B escolhida pelo hash das duas contas. O pedido vai
  assinado pelo aparelho de A, com o certificado. Republicado a cada 15 min por até 7 dias.
  B lê as 16 vagas a cada 90 s.
- **Aceite**: B passa a publicar a presença dele no canal do par. A, que tem o pedido
  pendente, lê esse canal; achou uma presença válida de B, virou amizade. Se os dois se
  adicionaram, vira amizade direto.
- **Presença**: nome, horário, endereços (público e na rede local, do socket da fofoca),
  a live atual (código, título, se é pública e se tem senha), o que está assistindo (o
  código só quando a transmissão é pública) e um convite, se houver. Republicada a cada
  4 min e na hora quando algo muda. Mais velha que 9 min (ou com `gone`, ao fechar) =
  offline. A de cada amigo é lida a cada minuto (a cada 5 com conexão direta).
- **Desfazer amizade**: publica uma presença com `removed`; o outro lado apaga também.

## Conexão direta

Com a presença do amigo em mãos, os dois mandam pings (mensagem `dm` da fofoca, cifrada com
o segredo do par e assinada pelo aparelho) para os endereços dele: quando os dois mandaram,
os roteadores deixam passar. Daí em diante a presença e os convites vão direto, na hora,
com um ping a cada 15 s para manter a passagem aberta. Sem resposta por 45 s, volta para a
DHT.

## Vários aparelhos

- **Vincular**: o aparelho novo sorteia a própria chave e uma chave efêmera, mostra um
  código de 6 dígitos e uma figura (3 formas coloridas), e publica o pedido na DHT numa
  chave derivada do código (salt `q`). O principal digita o código, lê o pedido e mostra a
  mesma figura; a pessoa confere que as duas batem (ninguém trocou o pedido no caminho) e
  confirma. O principal assina o certificado do aparelho novo e manda, cifrado para a chave
  efêmera dele (Diffie-Hellman + o código), a chave pública da conta, a chave de caixa, o
  certificado e a lista de amigos (salt `r0`…, em pedaços). O código vale 5 minutos.
- **Desvincular**: o principal põe a chave do aparelho na lista do cartão; os amigos leem o
  cartão a cada hora e passam a recusar o que ele assina.
- **Sincronizar**: os aparelhos da conta publicam a lista de amigos num canal da própria conta
  (cifra e chave de escrita derivadas da chave de caixa, salt `s0`…). Cada um lê, junta com a
  sua e publica de novo se mudou (a cada 2 min e quando a lista muda). Amizades desfeitas e
  pedidos cancelados ficam marcados com o horário, para a junção não trazê-los de volta.
- **Frase em outro PC**: restaurar pela frase deixa aquele PC principal também. A lista de
  amigos chega pela sincronização (se algum aparelho da conta esteve aberto nas últimas ~2 h).
  A lista da conta que o PC usava antes fica guardada à parte (`friends-<conta>.json`) e
  volta se ele voltar para ela.

## Na mesma live

O pedido para assistir e a resposta do host levam o código de amigo de cada um (só a chave
pública). Quem transmite vê "Adicionar" ao lado de cada espectador; quem assiste vê
"Adicionar" ao lado do host em "Você está assistindo". O botão manda um pedido de amizade
comum (pela caixa de entrada); se o outro já tinha pedido, vira "Aceitar".

## Na interface

- **Início**: as transmissões de amigos entram em "Ao vivo agora", primeiro, com a etiqueta
  "AMIGO" (e "SÓ AMIGOS" quando não estão no mural público).
- **Coluna da direita**: o bloco "Amigos" (pedidos recebidos com aceitar/recusar, quem está
  online com "Assistir", "Ir junto" ou "Convidar", e os offline recolhidos). Sem amigos, um
  convite com "Copiar meu código". Amigos aparecem em roxo (capa, etiqueta "AMIGO").
- **Campo de código** (barra do topo): aceita `AJ3.…` (assistir) e `AJF.…` (pedido de
  amizade). Ctrl+V fora de um campo de texto, no Início, entra com o código copiado. Um
  código copiado em outro app vira o aviso "Código copiado · Entrar" (só no PC; fica de fora
  o que o próprio app copiou, o seu código e o que já foi usado ou dispensado).
- **Você**: nome (editável no lugar), impressão; o avatar e o botão de conta abrem a janela
  da conta, em três abas: código de amigo; aparelhos (vincular, desvincular e entrar por
  outro aparelho); recuperação (a frase, e entrar com a frase de outra conta).
- **Convite**: um cartão no canto com "Assistir" e "Agora não".

## Limites (sem servidor)

- Perdeu a frase e todos os aparelhos principais: a conta não volta.
- Os amigos veem o IP uns dos outros (como no WebRTC e na fofoca).
- Desfazer amizade e recusar pedido valem só deste lado e do amigo; quem tem o seu código
  ainda pode mandar pedidos (recusados ficam ignorados).
- NAT simétrico (rede de celular) pode impedir a conexão direta: os avisos chegam pela DHT,
  em até um minuto.
- A presença no canal do par é uma só por conta: com dois aparelhos abertos, vale a do que
  publicou por último.
- Só o principal vincula e desvincula aparelhos. Um aparelho desvinculado ainda tem a chave de
  caixa (os amigos recusam o que ele assina, mas ele ainda consegue ler o que chega): para
  cortar de vez, crie uma conta nova.
- Quem você assiste (e quem te assiste) recebe o seu código de amigo, como recebe o seu nome.
- O segredo da conta fica em `identity.json` sem proteção extra do Windows, como as outras
  configurações.
