# The full arm with a 128k context window: compaction fires at a 96k budget.
agent = radian
provider = opencode-go
model = deepseek-v4.1-flash
extra = --profile full --approval auto --context-window 128000
