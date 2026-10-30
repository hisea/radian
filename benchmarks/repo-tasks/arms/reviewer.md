# The reviewer: an arm-like file read by review.sh. Read-only tools only.
agent = pi
provider = opencode-go
model = deepseek-v4.1-flash
extra = --no-extensions --no-skills --no-prompt-templates --no-mcp --tools read,grep,find,ls --thinking high
