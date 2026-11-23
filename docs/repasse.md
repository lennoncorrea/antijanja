# Repasse por outros usuários

Quando a conexão direta entre host e espectador não abre (os dois atrás de roteadores que
trocam a porta a cada destino, sem IPv6 e sem porta aberta), a conexão pode passar por um
terceiro: outro usuário do AntiJanja com a porta aberta no roteador. Ele roda um servidor
TURN pequeno dentro do próprio app (`src/relay.rs`). Nenhum servidor nosso.

## Quem repassa

- Opção "Ajudar quem não consegue conectar" nas configurações da transmissão, **ligada
  por padrão**. Só funciona de fato com UPnP e IP público: o app abre duas portas no
  roteador (a do servidor e a da conexão repassada, com prazo de 1 h, renovado).
- Limite: **uma conexão repassada por vez**, até **10 Mb/s** em cada sentido (o que
  passar disso é descartado, como numa internet cheia).
- Quem pode repassar avisa os vizinhos da fofoca (`relaycap`, repetido a cada troca de
  vizinhos). Ocupado, para de avisar.

## Quem usa

1. O espectador tenta a conexão direta, como sempre.
2. Se ela falhar antes de conectar, o app pede uma credencial a um vizinho que pode
   repassar (`relayreq` → `relayok` com o endereço do TURN, usuário e senha), avisa
   "Tentando pelo repasse de outro usuário…" e entra de novo com o repasse como caminho a
   mais. O ICE continua preferindo o caminho direto; o repasse só é usado se nada mais
   conectar.
3. Uma tentativa pelo repasse por transmissão a cada 3 min (sem ficar em loop).

## Segurança

- O conteúdo continua cifrado de ponta a ponta (DTLS/SRTP do WebRTC): quem repassa só vê
  pacotes passando, não o vídeo.
- Cada credencial vale para uma conexão só e some quando ela termina (ou em 60 s, se não
  for usada). Só vizinhos confirmados da fofoca podem pedir.
- Quem repassa vê o IP de quem usa o repasse (como qualquer vizinho da fofoca).

## Limites

- Precisa de pelo menos um vizinho na fofoca que possa repassar.
- Redes que bloqueiam UDP por completo continuam sem saída: o TURN da biblioteca de WebRTC
  só funciona por UDP, e sem UDP também não há DHT nem fofoca.
