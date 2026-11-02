# C ABI の使用例を作る。先に `cargo build -p senca-sdk` で libsenca_sdk.a / libsenca_sdk.so を作っておく。
#   make SDK_LIB=$CARGO_TARGET_DIR/debug
SDK_LIB ?= ../../../../target/debug
# 出力先（既定はこのディレクトリ）
OUT ?= .
CC ?= gcc
CFLAGS ?= -std=c11 -O2 -Wall -Wextra -Wpedantic -Werror
CPPFLAGS += -I../../include

all: $(OUT)/sender $(OUT)/sender-shared

# 静的リンク（Rust の標準ライブラリが使う pthread・dl・m を足す）
$(OUT)/sender: sender.c ../../include/senca.h $(SDK_LIB)/libsenca_sdk.a
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ sender.c $(SDK_LIB)/libsenca_sdk.a -lpthread -ldl -lm

# 共有ライブラリ
$(OUT)/sender-shared: sender.c ../../include/senca.h $(SDK_LIB)/libsenca_sdk.so
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ sender.c -L$(SDK_LIB) -lsenca_sdk -Wl,-rpath,$(abspath $(SDK_LIB))

clean:
	rm -f $(OUT)/sender $(OUT)/sender-shared

.PHONY: all clean
