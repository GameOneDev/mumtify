# Mumtify
Simple rust-based Mumble bot that uses `librespot` and `mumble-protocol` to play act as a device and play music in Mumble channel.

## Features
- Acts as a speaker in Spotify devices list
- Almost no stutters and sound lags
- Resource efficient
- Easy to use and configure

## How does it work?

1. Spotify Connect - Uses librespot to act as a Spotify Connect device called "MumbleDJ". Control playback from any Spotify app on your network.
2. Audio Pipeline - Captures Spotify audio, resamples it from 44.1kHz to 48kHz, encodes it to Opus format, and streams it to Mumble via encrypted UDP.
3. Mumble Integration - Connects to Mumble servers using the mumble-protocol library with TLS/UDP encryption for low-latency audio.
4. Volume Control - Responds to Spotify's native volume controls and Mumble text commands (!vol, !stop).

## Tech stack

- Rust (tokio async runtime)
- librespot (Spotify client)
- Opus (audio codec)
- rubato (resampling)
- mumble-protocol (server communication)

## Installation

### Building from source

Prerequisites:
- Rust 1.92.0
- Spotify Desktop/Mobile Client with Premium Subscription
- Mumble Certificate
1. Clone the repository 
```bash
git clone https://github.com/GameOneDev/mumtify
```
2. Install dependencies
``` bash
cargo run
```
3. Configure server, bot name and certificate in `src/main.rs`
4. Build release bot version
```bash
cargo build --release
```
5. Run Bot
```bash
chmod +x ./target/release/mumtify
./target/release/mumtify
```
7. Connect to the bot like you would connect to Spotify device
8. Play some music

### One liner Install

Installs `Rust`, Setups Repository, Interactively configures settings for you, Creates certificate, Installs all dependencies and Creates run scripts.
WIP

## How to Contribute

### Reporting Bugs

If you find a bug, please open an issue with: 
- A clear description of the problem
- Steps to reproduce
- Expected vs actual behavior
- Your environment (OS, Rust version, Mumble server version)
- Relevant logs (the bot uses `env_logger`)

### Suggesting Features

Feature requests are welcome! Please open an issue describing: 
- The feature and its use case
- Why it would be valuable
- Any implementation ideas you have

### Submitting Code

1. **Fork the repository** and create a new branch
   ```bash
   git checkout -b feature/your-feature-name
   ```

2. **Make your changes**
   - Follow Rust coding conventions
   - Add comments for complex logic
   - Test your changes thoroughly

3. **Run formatting and linting**
   ```bash
   cargo fmt
   cargo clippy
   ```

4. **Commit your changes**
   ```bash
   git commit -m "Add:  brief description of your changes"
   ```

5. **Push and create a Pull Request**
   ```bash
   git push origin feature/your-feature-name
   ```

## Development Guidelines

### Code Style
- Use `cargo fmt` to format code
- Run `cargo clippy` and address warnings
- Write clear, self-documenting code
- Add comments for non-obvious implementations

### Areas for Contribution

Here are some areas where contributions would be especially welcome: 

- **Configuration System**:  Add config file support (server address, credentials, audio settings)
- **Command System**: Expand text commands (queue, skip, playlist support)
- **Error Handling**:  Improve error messages and recovery
- **Documentation**: Add inline docs, usage examples, troubleshooting guides
- **Audio Quality**: Optimize resampling, reduce latency, improve buffer management
- **Multi-server Support**: Connect to multiple Mumble servers simultaneously
- **Web Interface**: Add a web dashboard for control and status
- **Testing**: Add unit and integration tests

### Commit Message Format

Use clear, descriptive commit messages:
- `Add: ` for new features
- `Fix:` for bug fixes
- `Update:` for improvements to existing features
- `Refactor:` for code restructuring
- `Docs:` for documentation changes

Example:  `Add: configuration file support for server settings`

## Testing

Currently, testing requires:
1. A running Mumble server
2. A Spotify Premium account
3. Manual testing of audio playback

When testing, verify:
- Bot connects to Mumble successfully
- Spotify Connect discovery works
- Audio plays without glitches or lag
- Volume control responds correctly
- Commands work as expected

## Questions? 

If you have questions or need help: 
- Open an issue for discussion
- Check existing issues and pull requests
- Review the code and inline comments

## License

By contributing to Mumtify, you agree that your contributions will be licensed under the same license as the project. 
