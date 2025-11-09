embedded:
	cargo build --features embedded --target xtensa-esp32-espidf -Z build-std="std,panic_abort"
uploader:
	cargo build --bin uploader --features upload --release
