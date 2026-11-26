# 可选：向量检索与重排

读者是编码助手。这是附加功能，默认关闭；不启用时关键词检索照常可用。**要不要启用、用本机还是云端，由用户决定。**

启用后，`recall` 变成“关键词 + 向量”，配了重排模型时再重排一次。模型出问题时自动退回关键词，输出的第一行会说明这次是怎么查的。

代价要先告诉用户：这个项目里有效经验的**标题和正文**（先脱敏）会发给所配置的模型服务，向量存在项目库里。记忆编号、文件路径、源码、意图和文件职责不发送。

## 1. 问用户选哪一种

- **不启用**（默认）。
- **本机**：需要本机装有 Ollama 并在运行；向量模型 bge-m3 约 1.2 GB，要下载。内容不出这台机器。
- **云端**：需要一个 OpenAI 兼容的向量接口的地址、模型名和密钥（如硅基流动）。经验的标题和正文会发到那个服务。

## 2a. 本机（Ollama）

```
loom config set embedding.base_url http://127.0.0.1:11434/v1
loom config set embedding.model bge-m3
loom model pull
loom model test
```

`model pull` 在模型不在本机时下载并显示进度，下载前把体积告诉用户。`model test` 报连接失败时，请用户启动 Ollama；不要替用户安装它或改它的设置。

## 2b. 云端

```
loom config set embedding.base_url <地址>
loom config set embedding.model <模型名>
```

例如地址 `https://api.siliconflow.cn/v1`、模型 `BAAI/bge-m3`。密钥二选一，**不要让用户把密钥发到对话里，也不要把它写进命令行参数或文件**：

- 用户已经有存着密钥的环境变量：`loom config set embedding.api_key_env <变量名>`
- 否则请用户自己在终端里运行 `loom config secret embedding`，按提示输入（不回显）。

然后 `loom model test`。

## 3. 可选：重排模型

重排接口的形状是 `POST <地址>/rerank`，硅基流动、Jina、Cohere 和 llama.cpp 的 llama-server 都是它。不配重排也可以，结果按关键词与向量的融合顺序给出。

- **云端**：

  ```
  loom config set rerank.base_url <地址>
  loom config set rerank.model <模型名>
  ```

  例如模型 `BAAI/bge-reranker-v2-m3`。密钥同上：`loom config set rerank.api_key_env <变量名>`，或用户自己运行 `loom config secret rerank`。
- **本机**：Ollama 没有重排接口。要在本机重排，用 llama-server 另起一个服务，例如 `llama-server -hf <重排模型的 GGUF 仓库> --reranking --port 8081`（首次启动会下载模型），然后：

  ```
  loom config set rerank.base_url http://127.0.0.1:8081/v1
  loom config set rerank.model bge-reranker-v2-m3
  ```

配完再 `loom model test`。

## 4. 对这个项目启用

```
loom semantic enable
```

这是预演：列出会发到哪里、发什么、有多少条。把它给用户看，用户同意后：

```
loom semantic enable --yes
loom semantic status
```

每个项目各自启用一次。之后把模型地址换到别的主机，这个项目要重新 `enable`。

## 日常

- `recall` 的输出提示还有经验没建向量时：`loom semantic index`。
- 第一行是“仅关键词（向量不可用：…）”时：`loom model test` 看原因并告诉用户。不要自行换模型或换地址。

## 停用

```
loom semantic disable --yes
```

这个项目回到纯关键词；加 `--purge` 同时删掉已建的向量。要去掉这台机器上的模型配置，逐项 `loom config unset <键>`，密钥用 `loom config secret embedding --clear`。
