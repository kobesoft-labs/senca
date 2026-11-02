// 作成・作り直しの応答でだけ渡される秘密（stream key）を、詳細画面へ渡すための置き場。
// メモリにだけ置き、1 回取り出したら消す（sessionStorage などには書かない）。

const box = new Map();

export function stashSecret(id, value) {
  box.set(id, value);
}

export function takeSecret(id) {
  const v = box.get(id);
  box.delete(id);
  return v || null;
}
