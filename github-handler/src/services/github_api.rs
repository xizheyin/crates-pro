use chrono::{Duration, NaiveDate};
use database::storage::Context;
use entity::{github_sync_status, programs};
use model::github::{Contributor, GitHubUser};
use reqwest::{header, Client, Error};
use sea_orm::{
    prelude::Uuid,
    ActiveValue::{NotSet, Set},
};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

// GitHub API URL
const GITHUB_API_URL: &str = "https://api.github.com";

// 使用main中定义的函数获取GitHub令牌
use crate::{config::get_github_token, services::model::GraphQLResponse};

use super::model::Repository;

// GitHub API客户端
pub struct GitHubApiClient {
    client: Client,
}

impl GitHubApiClient {
    // 创建新的GitHub API客户端
    pub fn new() -> Self {
        // 初始化为不带认证的Client
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("github-handler")
            .build()
            .unwrap_or_else(|_| Client::new());

        GitHubApiClient { client }
    }

    // 创建带有认证头的请求构建器
    fn authorized_request(&self, url: &str) -> reqwest::RequestBuilder {
        let token = get_github_token();
        let mut builder = self.client.get(url);

        if !token.is_empty() {
            builder = builder.header(header::AUTHORIZATION, format!("token {}", token));
        }

        builder.header(header::USER_AGENT, "github-handler")
    }

    // 获取GitHub用户详细信息
    pub async fn get_user_details(&self, username: &str) -> Result<GitHubUser, reqwest::Error> {
        let url = format!("{}/users/{}", GITHUB_API_URL, username);
        debug!("请求用户信息: {}", url);

        let response = self
            .authorized_request(&url)
            .send()
            .await?
            .error_for_status()?;
        let user: GitHubUser = response.json().await?;

        Ok(user)
    }

    // 获取所有仓库贡献者（通过Commits API）
    pub async fn get_all_repository_contributors(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<Contributor>, Box<dyn std::error::Error + Send + Sync>> {
        info!("通过Commits API获取所有仓库贡献者: {}/{}", owner, repo);

        // 使用HashMap统计每个贡献者的提交次数
        let mut contributors_map = std::collections::HashMap::new();
        let mut page = 1;
        let per_page = 100; // GitHub允许的最大值

        // 获取最近10,000个提交（100页，每页100个）
        let max_pages = 100;

        while page <= max_pages {
            let url = format!(
                "{}/repos/{}/{}/commits?page={}&per_page={}",
                GITHUB_API_URL, owner, repo, page, per_page
            );

            debug!("请求Commits API: {} (第{}页)", url, page);

            let response = match self.authorized_request(&url).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    warn!("获取提交页面 {} 失败: {}", page, e);
                    break;
                }
            };

            // 检查状态码
            if !response.status().is_success() {
                warn!("获取提交页面 {} 失败: HTTP {}", page, response.status());
                // 如果是速率限制，打印详细信息
                if response.status() == reqwest::StatusCode::FORBIDDEN {
                    if let Some(remain) = response.headers().get("x-ratelimit-remaining") {
                        warn!(
                            "GitHub API速率限制剩余: {}",
                            remain.to_str().unwrap_or("未知")
                        );
                    }
                    if let Some(reset) = response.headers().get("x-ratelimit-reset") {
                        let reset_time = reset.to_str().unwrap_or("0").parse::<i64>().unwrap_or(0);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        let wait_time = reset_time - now;
                        warn!(
                            "GitHub API速率限制重置时间: {} (还需等待约{}秒)",
                            reset_time,
                            if wait_time > 0 { wait_time } else { 0 }
                        );
                    }
                }
                break;
            }

            // 提取分页信息
            let has_next_page = response
                .headers()
                .get("link")
                .and_then(|h| h.to_str().ok())
                .map(|link| link.contains("rel=\"next\""))
                .unwrap_or(false);

            // 解析提交数据
            #[derive(Debug, Deserialize)]
            struct CommitAuthor {
                login: String,
                id: i64,
                avatar_url: String,
            }

            #[derive(Debug, Deserialize)]
            struct CommitInfo {
                _author: Option<String>,
                email: Option<String>,
            }

            #[derive(Debug, Deserialize)]
            struct CommitDetail {
                author: Option<CommitInfo>,
            }

            #[derive(Debug, Deserialize)]
            struct CommitData {
                author: Option<CommitAuthor>,
                commit: CommitDetail,
            }

            let commits: Vec<CommitData> = match response.json().await {
                Ok(c) => c,
                Err(e) => {
                    warn!("解析提交数据失败: {}", e);
                    break;
                }
            };

            if commits.is_empty() {
                info!("没有更多提交数据");
                break;
            }

            // 统计贡献者信息
            for commit in commits {
                // 获取提交中的电子邮箱
                let email = commit.commit.author.as_ref().and_then(|a| a.email.clone());

                if let Some(author) = commit.author {
                    contributors_map
                        .entry(author.id)
                        .and_modify(|e: &mut (String, String, i32, Option<String>)| {
                            e.2 += 1;
                            // 如果之前没有邮箱但现在有了，则更新
                            if e.3.is_none() && email.is_some() {
                                e.3 = email.clone();
                            }
                        })
                        .or_insert((author.login, author.avatar_url, 1, email));
                }
            }

            info!(
                "已处理 {} 页提交，当前贡献者数量: {}",
                page,
                contributors_map.len()
            );

            // 如果没有下一页，退出循环
            if !has_next_page {
                break;
            }

            // 添加延迟避免触发GitHub API限制
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            page += 1;
        }

        info!("通过Commits API找到 {} 名贡献者", contributors_map.len());

        // 转换为Contributor结构
        let mut commit_contributors = contributors_map
            .into_iter()
            .map(
                |(id, (login, avatar_url, contributions, email))| Contributor {
                    id,
                    login,
                    avatar_url,
                    contributions,
                    email,
                },
            )
            .collect::<Vec<_>>();

        // 按贡献数量排序
        commit_contributors.sort_by(|a, b| b.contributions.cmp(&a.contributions));

        Ok(commit_contributors)
    }

    pub async fn start_graphql_sync(&self, context: &Context) -> Result<(), Error> {
        let mut date = NaiveDate::parse_from_str("2011-01-01", "%Y-%m-%d").unwrap();
        let end_date = NaiveDate::parse_from_str("2025-04-01", "%Y-%m-%d").unwrap();
        let threshold_date = NaiveDate::parse_from_str("2015-01-01", "%Y-%m-%d").unwrap();

        // let mut date = start_date;
        while date <= end_date {
            let next_date = if date < threshold_date {
                date + Duration::days(60)
            } else {
                date + Duration::days(1)
            };

            tracing::info!(
                "Syncing date: {} to {}",
                date.format("%Y-%m-%d"),
                next_date.format("%Y-%m-%d")
            );

            self.sync_with_date(
                context,
                &date.format("%Y-%m-%d").to_string(),
                &next_date.format("%Y-%m-%d").to_string(),
            )
            .await?;
            date = next_date;
        }

        Ok(())
    }

    async fn sync_with_date(
        &self,
        context: &Context,
        start_date: &str,
        end_date: &str,
    ) -> Result<(), Error> {
        let sync_record = context
            .github_handler_stg()
            .get_github_sync_status_by_date(start_date, end_date)
            .await
            .unwrap();
        if let Some(record) = sync_record {
            if record.sync_result {
                return Ok(());
            }
        }
        const GITHUB_API_URL: &str = "https://api.github.com/graphql";

        let client = reqwest::Client::new();
        let mut cursor: Option<String> = None;

        loop {
            let query = r#"
        query ($query: String!, $cursor: String) {
            search(query: $query, type: REPOSITORY, first: 100, after: $cursor) {
                edges {
                    node {
                        ... on Repository {
                            name
                            url
                            createdAt
                        }
                    }
                }
                pageInfo {
                    endCursor
                    hasNextPage
                }
            }
        }
        "#;

            let variables = json!({ "query": format!("language:rust created:{}..{}", start_date, end_date), "cursor": cursor });

            let request_body = json!({
                "query": query,
                "variables": variables
            });

            let response = client
                .post(GITHUB_API_URL)
                .header("Authorization", format!("token {}", context.github_token))
                .header("User-Agent", "Rust-GraphQL-Client")
                .json(&request_body)
                .send()
                .await;
            let res = match response {
                Ok(response) => {
                    let status = response.status();
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "Failed to read response body".to_string());
                    tracing::info!("response body:{}", body);
                    if status.is_success() {
                        match serde_json::from_str::<GraphQLResponse>(&body) {
                            Ok(parsed) => Some(parsed),
                            Err(e) => {
                                tracing::error!(
                                    "❌ JSON Parse Error: {:?}\nRaw Response: {}",
                                    e,
                                    body
                                );
                                None
                            }
                        }
                    } else {
                        tracing::error!("❌ HTTP Error: {} - {}", status, body);
                        None
                    }
                }
                Err(err) => {
                    tracing::error!("❌ Request failed: {:?}", err);
                    None
                }
            };

            let mut save_models = vec![];

            if let Some(json) = res {
                match json.data {
                    Some(data) => {
                        for edge in data.search.edges {
                            convert_to_model(edge.node, &mut save_models).await;
                        }
                        context
                            .github_handler_stg()
                            .save_programs(save_models)
                            .await
                            .unwrap();
                        if data.search.page_info.has_next_page {
                            cursor = data.search.page_info.end_cursor;
                        } else {
                            break;
                        }
                    }
                    None => break,
                }
            } else {
                break;
            }
        }
        context
            .github_handler_stg()
            .save_github_sync_status(github_sync_status::ActiveModel {
                id: NotSet,
                start_date: Set(start_date.to_owned()),
                end_date: Set(end_date.to_owned()),
                sync_result: Set(true),
            })
            .await
            .unwrap();
        Ok(())
    }
}

async fn convert_to_model(item: Repository, save_models: &mut Vec<programs::ActiveModel>) {
    let model = programs::ActiveModel {
        id: Set(Uuid::new_v4()),
        github_url: Set(item.url),
        name: Set(item.name),
        description: Set("".to_owned()),
        namespace: Set("".to_owned()),
        max_version: Set("".to_owned()),
        mega_url: Set("".to_owned()),
        doc_url: Set("".to_owned()),
        program_type: Set("".to_owned()),
        downloads: Set(0),
        cratesio: Set("".to_owned()),
    };
    save_models.push(model);
}
